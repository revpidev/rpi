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

/// One reserved worker slot. Dropping it releases the slot, so a caller that
/// is cancelled after admission (or a sandbox that never starts its worker)
/// cannot leak capacity. The release callback handed to the sandbox owns this
/// value, which is why the slot outlives the caller-side handle until the
/// worker actually exits.
pub struct WorkerReservation {
    pool: Arc<WorkerPool>,
    released: bool,
}

impl WorkerReservation {
    fn new(pool: Arc<WorkerPool>) -> Self {
        Self {
            pool,
            released: false,
        }
    }

    /// Release the slot now. Equivalent to dropping the reservation, but
    /// explicit at the worker-exit boundary.
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if !self.released {
            self.released = true;
            self.pool.release();
        }
    }

    /// Return the slot without admitting waiters; the caller's drain loop
    /// continues iteratively.
    fn reclaim(mut self) {
        self.released = true;
        self.pool.reclaim_slot();
    }
}

impl Drop for WorkerReservation {
    fn drop(&mut self) {
        self.release_inner();
    }
}

impl std::fmt::Debug for WorkerReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerReservation")
            .field("released", &self.released)
            .finish()
    }
}

/// The outcome of one admission attempt.
#[derive(Debug)]
pub enum Admission {
    /// One worker slot is reserved. Dropping the reservation releases it; the
    /// caller normally moves it into the sandbox's release callback.
    Reserved(WorkerReservation),
    /// The caller's signal fired while waiting; nothing was reserved.
    Aborted,
    /// The wait deadline passed; nothing was reserved.
    Timeout,
}

struct Waiter {
    id: u64,
    /// The slot itself travels through the channel: a receiver dropped after
    /// a successful send drops the queued reservation, which releases the
    /// slot. A plain `()` would leak it.
    grant: oneshot::Sender<WorkerReservation>,
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

    /// Number of scripts waiting for a slot. Test/diagnostic helper.
    pub fn waiting(&self) -> usize {
        lock(&self.state).waiters.len()
    }

    /// Reserve one slot, waiting in arrival order while all are taken
    /// (`reserveCodemodeWorker`, execute.ts:75-95). Cancelling the wait does
    /// not release a worker that is already running.
    pub async fn reserve(
        self: &Arc<Self>,
        signal: Option<&CancellationToken>,
        timeout: Duration,
    ) -> Admission {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Admission::Aborted;
        }
        // Always join the queue and run the same FIFO drain as a release:
        // this is upstream `reserveCodemodeWorker` + `admitCodemodeWorkers`
        // (execute.ts:78-105), and it keeps an older waiter ahead of a
        // newcomer when the limit was raised in the meantime.
        let (id, receiver) = {
            let mut state = lock(&self.state);
            let id = self.next_waiter.fetch_add(1, Ordering::SeqCst);
            let (grant, receiver) = oneshot::channel();
            state.waiters.push_back(Waiter { id, grant });
            (id, receiver)
        };
        self.admit();
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);
        let aborted = async {
            match signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let mut receiver = receiver;
        let outcome = tokio::select! {
            biased;
            granted = &mut receiver => {
                return match granted {
                    Ok(reservation) => Admission::Reserved(reservation),
                    // The pool went away; nothing was reserved.
                    Err(_) => Admission::Aborted,
                };
            }
            () = &mut sleep => Admission::Timeout,
            () = aborted => Admission::Aborted,
        };
        // A grant can race the timeout/abort branch: if the waiter is gone
        // from the queue, its reservation is waiting in the channel.
        if self.remove_waiter(id) {
            outcome
        } else {
            match receiver.await {
                Ok(reservation) => Admission::Reserved(reservation),
                Err(_) => Admission::Aborted,
            }
        }
    }

    /// Release one slot and hand free capacity to waiters in arrival order
    /// (`releaseCodemodeWorker`, execute.ts:108-113). Only a
    /// [`WorkerReservation`] calls this.
    fn release(self: &Arc<Self>) {
        self.reclaim_slot();
        self.admit();
    }

    /// Drop one active count without admitting anyone; used when a grant
    /// cannot be delivered.
    fn reclaim_slot(&self) {
        let mut state = lock(&self.state);
        state.active = state.active.saturating_sub(1);
    }

    /// Give free slots to waiters in arrival order (`admitCodemodeWorkers`,
    /// execute.ts:96-105). Each grant carries the slot as a RAII reservation,
    /// so a receiver that disappears after the send still releases it.
    fn admit(self: &Arc<Self>) {
        loop {
            let grant = {
                let mut state = lock(&self.state);
                if state.waiters.is_empty() || state.active >= (self.limit)() {
                    None
                } else {
                    let Some(waiter) = state.waiters.pop_front() else {
                        break;
                    };
                    state.active += 1;
                    Some(waiter.grant)
                }
            };
            let Some(grant) = grant else {
                break;
            };
            let reservation = WorkerReservation::new(self.clone());
            if let Err(reservation) = grant.send(reservation) {
                // The receiver was dropped: return the slot without
                // recursing and try the next waiter.
                reservation.reclaim();
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

    fn reserved(admission: Admission) -> WorkerReservation {
        match admission {
            Admission::Reserved(reservation) => reservation,
            other => panic!("expected a reservation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admits_arrival_order_and_counts_each_slot_once() {
        let pool = Arc::new(WorkerPool::with_limit(|| 2));
        let first_slot = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        let second_slot = reserved(pool.reserve(None, Duration::from_millis(50)).await);
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
        drop(first_slot);
        let waiting_slot = reserved(first.await);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.as_mut())
                .await
                .is_err(),
            "the second waiter must still wait"
        );
        drop(waiting_slot);
        let last_slot = reserved(second.await);
        assert_eq!(pool.active(), 2);
        drop(second_slot);
        drop(last_slot);
        assert_eq!(pool.active(), 0);
    }

    #[tokio::test]
    async fn waits_with_a_deadline_and_a_cancellable_signal() {
        let pool = Arc::new(WorkerPool::with_limit(|| 1));
        let slot = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        assert!(matches!(
            pool.reserve(None, Duration::from_millis(10)).await,
            Admission::Timeout
        ));
        let signal = CancellationToken::new();
        let waiting = pool.reserve(Some(&signal), Duration::from_secs(5));
        let mut waiting = std::pin::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), waiting.as_mut())
                .await
                .is_err()
        );
        signal.cancel();
        assert!(matches!(waiting.await, Admission::Aborted));
        // The running worker keeps its slot; a cancelled waiter releases
        // nothing.
        assert_eq!(pool.active(), 1);
        drop(slot);
        assert_eq!(pool.active(), 0);
    }

    /// A caller that goes away after a grant must not leak the slot.
    #[tokio::test]
    async fn dropping_a_granted_reservation_releases_the_slot() {
        let pool = Arc::new(WorkerPool::with_limit(|| 1));
        let slot = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        let waiting = pool.reserve(None, Duration::from_secs(5));
        let mut waiting = std::pin::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), waiting.as_mut())
                .await
                .is_err()
        );
        drop(slot);
        // The waiting caller is granted, then goes away before using it.
        let granted = reserved(waiting.await);
        assert_eq!(pool.active(), 1);
        drop(granted);
        assert_eq!(pool.active(), 0);
    }

    /// Raising the limit while a waiter is queued admits the oldest waiter,
    /// not the newcomer (`admitCodemodeWorkers`, execute.ts:96-105).
    #[tokio::test]
    async fn a_raised_limit_grants_the_oldest_waiter_first() {
        let limit = Arc::new(AtomicUsize::new(2));
        let limit_for_pool = limit.clone();
        let pool = Arc::new(WorkerPool::with_limit(move || {
            limit_for_pool.load(Ordering::SeqCst)
        }));
        let slot_one = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        let slot_two = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        let mut older = Box::pin(pool.reserve(None, Duration::from_secs(5)));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), older.as_mut())
                .await
                .is_err()
        );
        limit.store(3, Ordering::SeqCst);
        // A newcomer arrives after the raise; the older waiter is still
        // ahead of it.
        let mut newcomer = Box::pin(pool.reserve(None, Duration::from_secs(5)));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), newcomer.as_mut())
                .await
                .is_err(),
            "the newcomer must not overtake the queued waiter"
        );
        let older_slot = reserved(older.await);
        assert_eq!(pool.active(), 3);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), newcomer.as_mut())
                .await
                .is_err(),
            "the newcomer still waits at the raised limit"
        );
        drop(older_slot);
        let newcomer_slot = reserved(newcomer.await);
        drop(slot_one);
        drop(slot_two);
        drop(newcomer_slot);
        assert_eq!(pool.active(), 0);
    }

    /// The original N4 race: a waiter is granted, then its future is dropped
    /// before it observes the reservation. The queued reservation must
    /// release the slot.
    #[tokio::test]
    async fn dropping_a_waiter_after_a_grant_does_not_leak_the_slot() {
        let pool = Arc::new(WorkerPool::with_limit(|| 1));
        let slot = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        let mut waiting = Box::pin(pool.reserve(None, Duration::from_secs(5)));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), waiting.as_mut())
                .await
                .is_err()
        );
        // Grant the waiter, then drop the future without polling it again.
        drop(slot);
        drop(waiting);
        assert_eq!(pool.active(), 0, "a dropped granted waiter leaked its slot");
        // The capacity is reusable.
        let next = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        assert_eq!(pool.active(), 1);
        drop(next);
    }

    /// A waiter dropped before the grant leaves a stale queue entry; the next
    /// release must hand the slot to the following waiter, not lose it.
    #[tokio::test]
    async fn a_waiter_dropped_while_queued_does_not_lose_the_next_grant() {
        let pool = Arc::new(WorkerPool::with_limit(|| 1));
        let slot = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        let mut stale = Box::pin(pool.reserve(None, Duration::from_secs(5)));
        let mut kept = Box::pin(pool.reserve(None, Duration::from_secs(5)));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stale.as_mut())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), kept.as_mut())
                .await
                .is_err()
        );
        drop(stale);
        drop(slot);
        let next = reserved(kept.await);
        assert_eq!(pool.active(), 1);
        drop(next);
        assert_eq!(pool.active(), 0);
    }

    #[tokio::test]
    async fn reads_the_limit_on_every_admission() {
        let limit = Arc::new(AtomicUsize::new(1));
        let limit_for_pool = limit.clone();
        let pool = Arc::new(WorkerPool::with_limit(move || {
            limit_for_pool.load(Ordering::SeqCst)
        }));
        let first = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        assert!(matches!(
            pool.reserve(None, Duration::from_millis(10)).await,
            Admission::Timeout
        ));
        limit.store(2, Ordering::SeqCst);
        // The next admission reads the raised limit.
        let second = reserved(pool.reserve(None, Duration::from_millis(50)).await);
        assert_eq!(pool.active(), 2);
        drop(first);
        drop(second);
        assert_eq!(pool.active(), 0);
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
