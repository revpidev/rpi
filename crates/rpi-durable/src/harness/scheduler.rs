//! Task-scheduling seam for the harness composition (port surface of
//! `packages/durable/src/harness/scheduler.ts` and `generation.ts`
//! @ `abe508e1b`).
//!
//! V17-14 splits the durable runtime across workers: the harness composition in
//! [`crate::harness::harness`] needs the scheduler for the operations that
//! reserve, run, abort, and wait on tasks, and [`crate::harness::submissions`]
//! needs `startRun()` to begin a generation run. Both are declared here as the
//! [`SchedulerService`] trait so the composition compiles before the scheduler
//! implementation lands.
//!
//! The scheduler/generation implementation (worker 3a) supplies the concrete
//! implementor. Until it is installed, [`open_harness`](crate::harness::open_harness)
//! uses [`UnavailableScheduler`], whose scheduler-backed operations reject with a
//! clear message while the wait-free paths (documents, submissions, inbox,
//! task-graph observation) keep working.
//!
//! Rust shape note (registered V17-14 §7.3): upstream `TaskScheduler` is a class
//! constructed directly by `HarnessImpl`; the port makes it an injected trait so
//! the composition and the scheduler can land independently. `AbortSignal.any`
//! composition becomes [`crate::chord::context`] cancellation tokens.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::chord::context::Context;
use crate::chord::json::JsonValue;
use crate::ids::{ConversationId, SubmissionId, TaskId};
use crate::session::{DocDraft, SessionError, SessionResult, Transaction};

use crate::harness::types::{RegistrySnapshot, Scheduling, SettledTask, TaskAbort, TaskInspection};

/// Cancellation and liveness handle bound to one task invocation
/// (upstream `InvocationBinding`, `scheduler.ts:109`).
///
/// Upstream carries an `AbortSignal` and a `check()` that throws once the
/// invocation ended; the Rust port carries a cancellation token and a fallible
/// check so a bound handle rejects through [`SessionError`].
#[derive(Clone)]
pub struct InvocationBinding {
    /// Cancelled when the invocation ends (per-invocation child token).
    pub signal: CancellationToken,
    /// Rejects once the invocation is no longer running.
    pub check: Arc<dyn Fn() -> SessionResult<()> + Send + Sync>,
}

/// Message every scheduler-backed operation reports while no scheduler is
/// installed (V17-14 seam: worker 3a supplies the implementation).
pub const UNAVAILABLE_MESSAGE: &str =
    "Task scheduling is unavailable: no SchedulerService is installed in this Harness";

fn unavailable<T>() -> SessionResult<T> {
    Err(SessionError::message(UNAVAILABLE_MESSAGE))
}

/// The harness's view of the task scheduler (upstream `TaskScheduler`).
///
/// The methods mirror the subset of `TaskScheduler` the composition calls:
/// `resume`/`open`/`join` for the lifecycle, `start_run`/`compact` for the
/// transaction-scoped work creation that `submissions.ts` and `harness.ts`
/// share with `generation.ts`/`compaction.ts`, and `abort`/`abort_conversation`/
/// `wait_for_task`/`wait_for_idle`/`inspect` for the public harness surface.
// FORM: upstream constructs a concrete `TaskScheduler`; the port injects this
// trait so the composition module compiles before the scheduler implementation.
#[async_trait]
pub trait SchedulerService: Send + Sync {
    /// Enable scheduling (idempotent); upstream `TaskScheduler.resume()`.
    fn resume(&self);

    /// Load live tasks and rewind surviving `running` tasks to `pending`
    /// (upstream `open()`); part of `Harness.open`.
    async fn open(&self, context: &Context) -> SessionResult<()>;

    /// Await every active invocation (upstream `join()`); part of `Harness.close`.
    async fn join(&self) -> SessionResult<()>;

    /// Record a new generation run and create its `pi.generation` task
    /// (upstream `startRun()` in `generation.ts:664`).
    async fn start_run(
        &self,
        tx: &mut Transaction,
        conversation_id: ConversationId,
        live: &DocDraft,
        inputs: Vec<SubmissionId>,
    ) -> SessionResult<()>;

    /// Create a manual compaction task (upstream `createCompaction()` in
    /// `compaction.ts`) and return its ID.
    async fn compact(
        &self,
        tx: &mut Transaction,
        conversation_id: ConversationId,
        instructions: Option<String>,
    ) -> SessionResult<TaskId>;

    /// Commit the abort mark and join the run invocation (upstream `abort()`).
    async fn abort(&self, id: TaskId, context: &Context) -> SessionResult<TaskAbort>;

    /// Withdraw queued inputs and abort the reachable live tasks of one
    /// conversation (upstream `abortConversation()`).
    async fn abort_conversation(
        &self,
        conversation_id: ConversationId,
        background: bool,
        context: &Context,
    ) -> SessionResult<()>;

    /// Resolve with the terminal receipt of one task (upstream `waitForTask`).
    async fn wait_for_task(
        &self,
        id: TaskId,
        context: &Context,
    ) -> SessionResult<SettledTask<JsonValue>>;

    /// Resolve when an ownerless conversation (or every one) is idle
    /// (upstream `waitForIdle`).
    async fn wait_for_idle(
        &self,
        conversation_id: Option<ConversationId>,
        context: &Context,
    ) -> SessionResult<()>;

    /// The scheduling state and live tasks the scheduler would act on
    /// (upstream `#inspect`).
    async fn inspect(
        &self,
        snapshot: Arc<dyn RegistrySnapshot>,
    ) -> SessionResult<(Scheduling, Vec<TaskInspection>)>;
}

/// Default [`SchedulerService`] used until worker 3a installs the real
/// scheduler. Every scheduler-backed operation rejects with
/// [`UNAVAILABLE_MESSAGE`]; `resume`/`join`/`open` are inert so the wait-free
/// harness lifecycle still works.
///
/// This is a deliberate V17-14 staging seam, not upstream behaviour: upstream
/// always constructs a live `TaskScheduler`.
pub struct UnavailableScheduler;

impl UnavailableScheduler {
    /// Create the placeholder scheduler.
    pub fn new() -> Self {
        Self
    }
}

impl Default for UnavailableScheduler {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl SchedulerService for UnavailableScheduler {
    fn resume(&self) {}

    async fn open(&self, _context: &Context) -> SessionResult<()> {
        Ok(())
    }

    async fn join(&self) -> SessionResult<()> {
        Ok(())
    }

    async fn start_run(
        &self,
        _tx: &mut Transaction,
        _conversation_id: ConversationId,
        _live: &DocDraft,
        _inputs: Vec<SubmissionId>,
    ) -> SessionResult<()> {
        unavailable()
    }

    async fn compact(
        &self,
        _tx: &mut Transaction,
        _conversation_id: ConversationId,
        _instructions: Option<String>,
    ) -> SessionResult<TaskId> {
        unavailable()
    }

    async fn abort(&self, _id: TaskId, _context: &Context) -> SessionResult<TaskAbort> {
        unavailable()
    }

    async fn abort_conversation(
        &self,
        _conversation_id: ConversationId,
        _background: bool,
        _context: &Context,
    ) -> SessionResult<()> {
        unavailable()
    }

    async fn wait_for_task(
        &self,
        _id: TaskId,
        _context: &Context,
    ) -> SessionResult<SettledTask<JsonValue>> {
        unavailable()
    }

    async fn wait_for_idle(
        &self,
        _conversation_id: Option<ConversationId>,
        _context: &Context,
    ) -> SessionResult<()> {
        unavailable()
    }

    async fn inspect(
        &self,
        _snapshot: Arc<dyn RegistrySnapshot>,
    ) -> SessionResult<(Scheduling, Vec<TaskInspection>)> {
        // Inspection is a read; report a paused scheduler with no live tasks
        // rather than failing, so `Harness.inspect` still lists submissions.
        Ok((Scheduling::Paused, Vec::new()))
    }
}

/// A scheduler factory receiving the owning session (upstream constructs the
/// `TaskScheduler` with the harness as its session).
pub type SchedulerFactory =
    Arc<dyn Fn(&Arc<crate::session::SessionImpl>) -> Arc<dyn SchedulerService> + Send + Sync>;
