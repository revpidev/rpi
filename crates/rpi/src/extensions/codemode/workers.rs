//! Process-wide codemode worker capacity (port of the admission queue in
//! `packages/coding-agent/src/extensions/codemode/execute.ts:55-110`
//! @ 4a080252f).
//!
//! Every script, including those started by in-process subagents, shares one
//! queue in this process. A worker holds its slot until its thread actually
//! exits, so a cancelled or timed-out script that is still shutting down
//! keeps counting. When all slots are taken, scripts wait in arrival order
//! for at most [`WORKER_WAIT_MS`] (or their `timeout_ms`, whichever is
//! shorter), and the wait is cancellable.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Default worker limit (execute.ts:58).
pub const DEFAULT_MAX_WORKERS: usize = 2;
/// How long a script waits for a free worker before failing (execute.ts:69).
pub const WORKER_WAIT_MS: u64 = 60_000;
/// Product configuration key (ADR-0001 maps upstream `PI_` to `RPI_`).
pub const MAX_WORKERS_ENV: &str = "RPI_CODEMODE_MAX_WORKERS";

/// `maxCodemodeWorkers` (execute.ts:60-64): the environment value if it is a
/// positive integer, else the default. Read on every admission and every
/// grant so a host can change it at runtime.
pub fn max_workers() -> usize {
    parse_max_workers(std::env::var(MAX_WORKERS_ENV).ok().as_deref())
}

/// The pure limit parser: a positive integer, else the default.
fn parse_max_workers(raw: Option<&str>) -> usize {
    let Some(raw) = raw else {
        return DEFAULT_MAX_WORKERS;
    };
    let Ok(value) = raw.trim().parse::<f64>() else {
        return DEFAULT_MAX_WORKERS;
    };
    if value.is_finite() && value.fract() == 0.0 && value > 0.0 && value <= usize::MAX as f64 {
        value as usize
    } else {
        DEFAULT_MAX_WORKERS
    }
}

/// The outcome of one admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// One worker slot is reserved; the caller must pass its release callback
    /// to the sandbox.
    Reserved,
    /// The caller's signal fired while waiting; nothing was reserved.
    Aborted,
    /// The wait deadline passed; nothing was reserved.
    Timeout,
}

struct Waiter {
    id: u64,
    grant: oneshot::Sender<()>,
}

#[derive(Default)]
struct PoolState {
    active: usize,
    waiters: VecDeque<Waiter>,
}

/// A shared worker-capacity queue (`admitCodemodeWorkers`,
/// `releaseCodemodeWorker`, execute.ts:96-113).
pub struct WorkerPool {
    limit: Arc<dyn Fn() -> usize + Send + Sync>,
    state: Mutex<PoolState>,
    next_waiter: AtomicU64,
}

impl WorkerPool {
    /// A pool whose limit comes from [`MAX_WORKERS_ENV`] at every admission.
    pub fn from_env() -> Self {
        Self::with_limit(max_workers)
    }

    /// A pool with an explicit limit source (unit tests).
    pub fn with_limit(limit: impl Fn() -> usize + Send + Sync + 'static) -> Self {
        Self {
            limit: Arc::new(limit),
            state: Mutex::new(PoolState::default()),
            next_waiter: AtomicU64::new(1),
        }
    }

    /// Number of slots currently held. Test/diagnostic helper.
    pub fn active(&self) -> usize {
        lock(&self.state).active
    }

    /// Reserve one slot, waiting in arrival order while all are taken
    /// (`reserveCodemodeWorker`, execute.ts:75-95). Cancelling the wait does
    /// not release a worker that is already running.
    pub async fn reserve(
        &self,
        signal: Option<&CancellationToken>,
        timeout: Duration,
    ) -> Admission {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Admission::Aborted;
        }
        let (id, receiver) = {
            let mut state = lock(&self.state);
            if state.active < (self.limit)() {
                state.active += 1;
                return Admission::Reserved;
            }
            let id = self.next_waiter.fetch_add(1, Ordering::SeqCst);
            let (grant, receiver) = oneshot::channel();
            state.waiters.push_back(Waiter { id, grant });
            (id, receiver)
        };
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);
        let aborted = async {
            match signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let outcome = tokio::select! {
            biased;
            _ = receiver => return Admission::Reserved,
            () = &mut sleep => Admission::Timeout,
            () = aborted => Admission::Aborted,
        };
        // A grant can race the timeout/abort branch: if the waiter is gone
        // from the queue, admission already granted the slot to us.
        if self.remove_waiter(id) {
            outcome
        } else {
            Admission::Reserved
        }
    }

    /// Release one slot and hand free capacity to waiters in arrival order
    /// (`releaseCodemodeWorker`, execute.ts:108-113).
    pub fn release(&self) {
        let mut state = lock(&self.state);
        state.active = state.active.saturating_sub(1);
        while !state.waiters.is_empty() && state.active < (self.limit)() {
            let Some(waiter) = state.waiters.pop_front() else {
                break;
            };
            state.active += 1;
            // A dropped receiver (the reserving future was cancelled) must
            // not leak the slot.
            if waiter.grant.send(()).is_err() {
                state.active -= 1;
            }
        }
    }

    /// Remove a waiter that timed out or was cancelled. `false` when a
    /// concurrent release already granted it.
    fn remove_waiter(&self, id: u64) -> bool {
        let mut state = lock(&self.state);
        let before = state.waiters.len();
        state.waiters.retain(|waiter| waiter.id != id);
        state.waiters.len() != before
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// The process-wide pool shared by every in-process codemode caller,
/// including in-process subagents.
pub fn pool() -> Arc<WorkerPool> {
    static POOL: OnceLock<Arc<WorkerPool>> = OnceLock::new();
    POOL.get_or_init(|| Arc::new(WorkerPool::from_env()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn admits_arrival_order_and_counts_each_slot_once() {
        let pool = WorkerPool::with_limit(|| 2);
        assert_eq!(
            pool.reserve(None, Duration::from_millis(50)).await,
            Admission::Reserved
        );
        assert_eq!(
            pool.reserve(None, Duration::from_millis(50)).await,
            Admission::Reserved
        );
        let first = pool.reserve(None, Duration::from_secs(5));
        let second = pool.reserve(None, Duration::from_secs(5));
        // Poll both waiters into the queue, then release one slot: the
        // earlier waiter is the one admitted.
        let mut first = std::pin::pin!(first);
        let mut second = std::pin::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), first.as_mut())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), second.as_mut())
                .await
                .is_err()
        );
        pool.release();
        assert_eq!(first.await, Admission::Reserved);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.as_mut())
                .await
                .is_err(),
            "the second waiter must still wait"
        );
        pool.release();
        assert_eq!(second.await, Admission::Reserved);
        assert_eq!(pool.active(), 2);
        pool.release();
        pool.release();
        assert_eq!(pool.active(), 0);
    }

    #[tokio::test]
    async fn waits_with_a_deadline_and_a_cancellable_signal() {
        let pool = WorkerPool::with_limit(|| 1);
        assert_eq!(
            pool.reserve(None, Duration::from_millis(50)).await,
            Admission::Reserved
        );
        assert_eq!(
            pool.reserve(None, Duration::from_millis(10)).await,
            Admission::Timeout
        );
        let signal = CancellationToken::new();
        let waiting = pool.reserve(Some(&signal), Duration::from_secs(5));
        let mut waiting = std::pin::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), waiting.as_mut())
                .await
                .is_err()
        );
        signal.cancel();
        assert_eq!(waiting.await, Admission::Aborted);
        // The running worker keeps its slot; a cancelled waiter releases
        // nothing.
        assert_eq!(pool.active(), 1);
        pool.release();
    }

    #[tokio::test]
    async fn reads_the_limit_on_every_admission() {
        let limit = Arc::new(AtomicUsize::new(1));
        let limit_for_pool = limit.clone();
        let pool = WorkerPool::with_limit(move || limit_for_pool.load(Ordering::SeqCst));
        assert_eq!(
            pool.reserve(None, Duration::from_millis(50)).await,
            Admission::Reserved
        );
        assert_eq!(
            pool.reserve(None, Duration::from_millis(10)).await,
            Admission::Timeout
        );
        limit.store(2, Ordering::SeqCst);
        // The next admission reads the raised limit, and a waiting caller is
        // admitted by the next release.
        assert_eq!(
            pool.reserve(None, Duration::from_millis(50)).await,
            Admission::Reserved
        );
        assert_eq!(pool.active(), 2);
        pool.release();
        pool.release();
    }

    #[test]
    fn parses_the_worker_limit_and_falls_back_to_two() {
        assert_eq!(parse_max_workers(None), DEFAULT_MAX_WORKERS);
        assert_eq!(parse_max_workers(Some("1")), 1);
        assert_eq!(parse_max_workers(Some("3")), 3);
        assert_eq!(parse_max_workers(Some(" 4 ")), 4);
        assert_eq!(parse_max_workers(Some("1e3")), 1000);
        for invalid in ["", "0", "-1", "2.5", "NaN", "Infinity", "-Infinity", "abc"] {
            assert_eq!(
                parse_max_workers(Some(invalid)),
                DEFAULT_MAX_WORKERS,
                "{invalid:?}"
            );
        }
    }
}
