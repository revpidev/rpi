//! Task scheduler of one Harness (port of
//! `packages/durable/src/harness/scheduler.ts` @ `4a080252f`).
//!
//! [`TaskScheduler`] mirrors every committed non-terminal task (`pending`,
//! `running`, `waiting`, `completing`) in memory; the synchronous commit
//! listener updates that mirror on the Session line, so code running on the line
//! reads exactly the committed state. Tasks and conversations form one ownership
//! tree; walks up it decide cascades, idle scopes, and whether a task's ordinary
//! owned work is live (which holds its outcome as `completing` and delays its
//! abort handler).
//!
//! Invariant: every task transition is decided and written by one callback
//! serialized on the Session line. That covers reservation, marks, runtime
//! commits, finalization, and the synchronous step before each phase, which
//! applies the precedence rules and writes a fault or a handover. Handlers and
//! joins run off the line; an invocation ends inside the step that decides its
//! end, so a runtime commit it queued either lands before that decision or is
//! rejected.
//!
//! Rust shape notes (registered V17-14 §7.3): `queueMicrotask` kicks become a
//! `dirty` flag plus a spawned drain task; `AbortController`/`AbortSignal`
//! becomes [`tokio_util::sync::CancellationToken`] with Chord composition;
//! `Promise.withResolvers` waiters become [`crate::harness::util::Waiters`];
//! `Map`/`Set` iteration order is a `BTreeMap`/`BTreeSet`. Upstream phases are an
//! object indexed by `checkpoint.phase`; the port dispatches inside
//! [`TaskDefinition::run_phase`](crate::harness::types::TaskDefinition::run_phase).
//! The context-retention timer is not reproduced: expiry is rechecked at every
//! settle-idle pass (no unref'd timer exists here; V17-14 §7.3 shape difference).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_ai::models::Models;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::chord::context::{Context, await_with_context, with_abort_signal};
use crate::chord::json::{JsonObject, JsonValue};
use crate::chord::telemetry::TelemetrySpan;
use crate::documents::DocToken;
use crate::harness::context::{ContextRange, read_context_from};
use crate::harness::live::{LiveRun, LiveState, SchedulerOutcome, settle_scheduler_outcome};
use crate::harness::registry::{COMPACTION_TASK_NAME, GENERATION_TASK_NAME};
use crate::harness::submissions::Submissions;
use crate::harness::types::{
    Agent, AgentState, AnyTask, BlockedReason, DocumentReader, EnvFactory, EnvTarget, ExecutionEnv,
    Migration, ModelsSource, NextTaskState, RegistryReader, RegistrySnapshot, ReportFn,
    RunningTask, Scheduling, Settings, SettingsSource, SettledTask, TaskAbort, TaskCommitFn,
    TaskInspection, TaskInspectionState, TaskRuntime, TimerDisarm, Wake, WakeFn, read_state,
    write_state,
};
use crate::harness::util::{WaitFuture, Waiters, closed_error};
use crate::ids::{ConversationId, EntryId, SubmissionId, TaskId};
use crate::session::{
    CommitChange, CommitPublication, CommittedWatch, DocDraft, DocOwner, NowFn, SessionError,
    SessionImpl, SessionResult, Subscription, TaskCreate, TaskOwnership, Transaction,
    TransactionScope, commit_fn,
};
use crate::timers::Timers;
// The scheduler couples to the conversation host through `bound_conversation`,
// which composes the invocation-bound handle from the composition module.
use crate::harness::harness::bound_conversation;
use crate::harness::types::ConversationHandle;
use crate::types::{
    JoinPolicy, Storage, StorageWrite, SubmissionQuery, SubmissionRecord, SubmissionStatusFilter,
    TaskOutcome, TaskOutcomeError, TaskQuery, TaskRecord, TaskState, TaskStatus,
};

/// Page size of scheduler scans (upstream `SCAN_PAGE_SIZE`).
const SCAN_PAGE_SIZE: usize = 256;

/// What `blocked_reason()` reads of a task: the task graph keeps only this of
/// each record (upstream `BlockedSubject`).
#[derive(Debug, Clone)]
pub struct BlockedSubject {
    /// Task identity.
    pub id: TaskId,
    /// Registered task name.
    pub kind: String,
    /// Stored definition version.
    pub version: u64,
    /// Durable abort mark.
    pub abort_requested: bool,
    /// Mark reason when the abort was not requested by the host.
    pub abort_reason: Option<String>,
    /// Consecutive restarts without progress.
    pub restarts: Option<u32>,
    /// Committed lifecycle status.
    pub status: TaskStatus,
}

impl BlockedSubject {
    /// The part of one record the task graph keeps.
    pub fn of(record: &TaskRecord) -> Self {
        Self {
            id: record.id,
            kind: record.kind.clone(),
            version: record.version,
            abort_requested: record.abort_requested,
            abort_reason: record.abort_reason.clone(),
            restarts: record.restarts,
            status: record.status(),
        }
    }
}

/// Listener notified when a blocked reason may have changed without a commit.
pub type BlockedListener = Arc<dyn Fn() + Send + Sync>;
/// Message every scheduler-backed operation reports while no scheduler is
/// installed (V17-14 staging seam).
pub const UNAVAILABLE_MESSAGE: &str =
    "Task scheduling is unavailable: no SchedulerService is installed in this Harness";

fn unavailable<T>() -> SessionResult<T> {
    Err(SessionError::message(UNAVAILABLE_MESSAGE))
}

/// The error a runtime operation rejects with once an invocation ended
/// (upstream `endedError`).
fn ended_error(task_id: TaskId) -> SessionError {
    SessionError::message(format!("Task {task_id} invocation has ended"))
}

/// Cancellation and liveness handle bound to one task invocation
/// (upstream `InvocationBinding`, `scheduler.ts:109`).
#[derive(Clone)]
pub struct InvocationBinding {
    /// Cancelled when the invocation ends (per-invocation child token).
    pub signal: CancellationToken,
    /// Rejects once the invocation is no longer running.
    pub check: Arc<dyn Fn() -> SessionResult<()> + Send + Sync>,
}

/// The harness's view of the task scheduler (upstream `TaskScheduler`).
// FORM: upstream constructs a concrete `TaskScheduler`; the port injects this
// trait so the composition and the scheduler land independently.
#[async_trait]
pub trait SchedulerService: Send + Sync {
    /// Enable scheduling (idempotent); upstream `TaskScheduler.resume()`.
    fn resume(&self);

    /// When the Session next needs a running process.
    fn wake(&self) -> Wake;

    /// Run a task parked by the restart limit again.
    async fn retry_task(&self, id: TaskId, context: &Context) -> SessionResult<()>;

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

    /// Create a manual compaction task and return its ID.
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

    /// Why a live task cannot run here, if it cannot: its definition (missing,
    /// too old, failed migration) or the restart limit. A running task, an
    /// abort-marked one, and one holding its outcome are never blocked.
    fn blocked_reason(&self, _subject: &BlockedSubject) -> Option<BlockedReason> {
        None
    }

    /// Call `listener` when blocked reasons may have changed without a commit: a
    /// registry change or a failed migration.
    fn subscribe_blocked(&self, _listener: BlockedListener) -> Subscription {
        Subscription::new(|| {})
    }

    /// Bind the owning submissions so an invocation-bound conversation handle
    /// can admit input. Default: no-op (the recording double needs none).
    fn install_submissions(&self, submissions: Arc<Submissions>) {
        let _ = submissions;
    }
}

/// Everything the real scheduler needs from its owning Harness
/// (upstream `TaskSchedulerOptions`).
pub struct SchedulerDeps {
    /// Owning Session kernel.
    pub session: Arc<SessionImpl>,
    /// Storage backend.
    pub storage: Arc<dyn Storage>,
    /// pi-ai model catalog.
    pub models: ModelsSource,
    /// Application-owned registry reader.
    pub registry: Arc<dyn RegistryReader>,
    /// Resolved harness settings.
    pub settings: SettingsSource,
    /// Environment factory.
    pub env: Option<EnvFactory>,
    /// Receives extension failures.
    pub report: ReportFn,
    /// Fixed-field report callback for the host. The scheduler emits the
    /// report's telemetry itself before calling it.
    pub report_info: Option<crate::harness::types::ReportInfoFn>,
    /// Wall clock.
    pub now: NowFn,
    /// Host timers.
    pub set_timer: Option<crate::harness::types::SetTimer>,
    /// Host wake callback, replacing the timer for sleeping tasks.
    pub on_wake: Option<WakeFn>,
    /// Host-owned content-free telemetry.
    pub telemetry: Option<crate::chord::telemetry::TelemetryContext>,
    /// Context of scheduler commits and invocations, without caller cancellation.
    pub context: Context,
}

/// A scheduler factory receiving the owning Harness internals.
pub type SchedulerFactory = Arc<dyn Fn(SchedulerDeps) -> Arc<dyn SchedulerService> + Send + Sync>;

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Whether one invocation runs task code or its abort handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvocationMode {
    /// Runs phase handlers.
    Run,
    /// Runs the abort handler.
    Abort,
}

/// One in-memory execution of a task (upstream `Invocation`).
struct Invocation {
    task_id: TaskId,
    conversation_id: ConversationId,
    mode: InvocationMode,
    /// Adapter-owned parent carrier stored on the task record.
    trace: Option<crate::chord::telemetry::TelemetryCarrier>,
    /// Immutable creation-time links, attached to every phase span.
    links: Option<Vec<crate::chord::telemetry::TelemetryCarrier>>,
    /// Terminal outcome committed by this invocation, for the phase span.
    terminal_outcome: Mutex<Option<TaskOutcome>>,
    controller: CancellationToken,
    /// Context passed to handlers; cancelled by `controller`.
    context: Context,
    /// Watches acquired through the runtime; stopped at invocation end.
    watches: Mutex<Vec<CommittedWatch>>,
    /// Timers armed through the runtime; disarmed at invocation end.
    timers: Mutex<Vec<(u64, TimerDisarm)>>,
    next_timer_id: AtomicU64,
    ended: AtomicBool,
    done_tx: tokio::sync::watch::Sender<bool>,
    notify: Notify,
}

impl Invocation {
    fn finish(&self) {
        let _ = self.done_tx.send(true);
        self.notify.notify_waiters();
    }
}

/// Wait until one invocation ends.
async fn wait_invocation(invocation: &Arc<Invocation>) {
    loop {
        if invocation.ended.load(Ordering::SeqCst) {
            return;
        }
        let notified = invocation.notify.notified();
        tokio::pin!(notified);
        // Register before checking: `notify_waiters` stores no permit.
        notified.as_mut().enable();
        if invocation.ended.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

/// Clears the reconcile-in-progress marker even if the pass panics.
struct ReconcilingGuard<'a>(&'a AtomicBool);

impl Drop for ReconcilingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// One reservation returned by a scheduling pass (upstream `Reservation`).
#[derive(Clone)]
struct Reservation {
    invocation: Arc<Invocation>,
    task: AnyTask,
    snapshot: Arc<dyn RegistrySnapshot>,
}

/// The ownership fields of a task (upstream `TaskNode`).
#[derive(Debug, Clone)]
struct TaskNode {
    conversation_id: ConversationId,
    owner: Option<TaskId>,
    background: bool,
}

/// Where a walk up the ownership tree continues (upstream `Up`).
#[derive(Debug, Clone, Copy)]
enum Up {
    Task(TaskId),
    Conversation(ConversationId),
}

/// One step of a walk up (upstream `Step`).
enum Step {
    Task(TaskId, TaskNode),
    Conversation(ConversationId),
    Unknown,
}

/// Where ordinary ownership traversal starts (upstream `Scope`).
enum Scope {
    Conversation(ConversationId),
    Roots,
}

/// Candidate records a commit staged (upstream `Overlay`).
struct Overlay {
    tasks: BTreeMap<TaskId, TaskRecord>,
    edges: BTreeMap<ConversationId, Option<TaskId>>,
}

/// Outcome of resolving a record against a registry snapshot (upstream `Resolution`).
enum Resolution {
    Ready {
        task: AnyTask,
        record: Box<TaskRecord>,
    },
    Blocked {
        reason: BlockedReason,
    },
}

/// Whether a definition can take a record and whether it needs migration
/// (upstream `Fit`).
enum Fit {
    Ready {
        task: AnyTask,
        migrates: bool,
    },
    Blocked {
        reason: BlockedReason,
        error: Option<String>,
    },
}

/// What a step decided (upstream `Decision`).
#[derive(Clone)]
enum Decision {
    Continue,
    End,
    Fault(String),
}

/// Outcome of the phase that just returned (upstream `PhaseResult`).
#[derive(Clone)]
struct PhaseResult {
    checkpoint: JsonValue,
    phase: String,
    failure: Option<String>,
}

/// The decision a step applies (upstream `#step` parameter).
#[derive(Clone)]
enum Decider {
    Run {
        previous: Option<PhaseResult>,
        task: AnyTask,
        snapshot: Arc<Mutex<Arc<dyn RegistrySnapshot>>>,
        reported: Arc<Mutex<Option<Option<AnyTask>>>>,
    },
    Abort {
        failure: Option<String>,
    },
}

/// One kept context range of a conversation (upstream `#contexts` entry).
#[derive(Clone)]
struct KeptContext {
    range: ContextRange,
    idle_since: Option<f64>,
}

/// Outcome of `abort` staged inside its commit (upstream inline return).
struct AbortCommit {
    result: TaskAbort,
    run: Option<Arc<Invocation>>,
}

/// Shared scheduler state. Methods take `self: &Arc<Self>` so they can spawn.
struct SchedulerInner {
    session: Arc<SessionImpl>,
    storage: Arc<dyn Storage>,
    models: ModelsSource,
    registry: Arc<dyn RegistryReader>,
    settings: SettingsSource,
    env: Option<EnvFactory>,
    report: ReportFn,
    report_info: Option<crate::harness::types::ReportInfoFn>,
    now: NowFn,
    timers: Arc<Timers>,
    on_wake: Option<WakeFn>,
    telemetry: crate::chord::telemetry::TelemetryContext,
    context: Context,
    /// Earliest sleep timer, without `onWake` while scheduling runs.
    wake_timer: Mutex<Option<(f64, TimerDisarm)>>,
    /// Background timer for the earliest idle-context expiry.
    expiry_timer: Mutex<Option<(f64, TimerDisarm)>>,
    /// Last value given to `onWake`, so an unchanged one is not repeated.
    last_wake: Mutex<Option<Wake>>,
    submissions: std::sync::OnceLock<Arc<Submissions>>,
    live: Mutex<BTreeMap<TaskId, TaskRecord>>,
    contexts: Mutex<BTreeMap<ConversationId, KeptContext>>,
    invocations: Mutex<BTreeMap<TaskId, Arc<Invocation>>>,
    task_waiters: Waiters<TaskId, SettledTask<JsonValue>>,
    idle_waiters: Waiters<Option<ConversationId>, ()>,
    /// Definition whose migration failed per task; retried only for another definition.
    failed_migrations: Mutex<BTreeMap<TaskId, (AnyTask, String)>>,
    /// Tasks loaded by this Harness that have not run their first phase yet.
    resumed: Mutex<BTreeSet<TaskId>>,
    /// A panicking registry snapshot was reported once.
    registry_snapshot_reported: AtomicBool,
    /// Task-graph listeners for blocked reasons that changed without a commit.
    blocked_listeners: Mutex<BTreeMap<u64, BlockedListener>>,
    /// Next blocked-listener id.
    next_blocked_listener: std::sync::atomic::AtomicU64,
    /// Owner task of each loaded conversation, `None` when ownerless.
    edges: Mutex<BTreeMap<ConversationId, Option<TaskId>>>,
    /// Tasks that own a loaded conversation.
    conversation_owners: Mutex<BTreeSet<TaskId>>,
    /// Ownership fields of terminal tasks that walks pass through.
    settled: Mutex<BTreeMap<TaskId, TaskNode>>,
    /// `failFast` waiters the next reconcile checks for a failed task in `on`.
    fail_fast_checks: Mutex<BTreeSet<TaskId>>,
    cascade_pending: AtomicBool,
    reconcile_scheduled: AtomicBool,
    /// A reconcile pass is running; work that must land before the host may idle.
    reconciling: AtomicBool,
    /// One `onWake` delivery is queued on the runtime.
    wake_scheduled: AtomicBool,
    unsubscribe_registry: Mutex<Option<Subscription>>,
    enabled: AtomicBool,
    closing: AtomicBool,
    dirty: AtomicBool,
    draining: AtomicBool,
}

impl SchedulerInner {
    fn new(deps: SchedulerDeps) -> Self {
        Self {
            session: deps.session,
            storage: deps.storage,
            models: deps.models,
            registry: deps.registry,
            settings: deps.settings,
            env: deps.env,
            report: Arc::clone(&deps.report),
            report_info: deps.report_info,
            now: deps.now,
            timers: Timers::new(deps.set_timer, Arc::clone(&deps.report)),
            on_wake: deps.on_wake,
            telemetry: deps
                .telemetry
                .unwrap_or_else(crate::chord::telemetry::TelemetryContext::none),
            context: deps.context,
            wake_timer: Mutex::new(None),
            expiry_timer: Mutex::new(None),
            last_wake: Mutex::new(None),
            submissions: std::sync::OnceLock::new(),
            live: Mutex::new(BTreeMap::new()),
            contexts: Mutex::new(BTreeMap::new()),
            invocations: Mutex::new(BTreeMap::new()),
            task_waiters: Waiters::new(),
            idle_waiters: Waiters::new(),
            failed_migrations: Mutex::new(BTreeMap::new()),
            resumed: Mutex::new(BTreeSet::new()),
            registry_snapshot_reported: AtomicBool::new(false),
            blocked_listeners: Mutex::new(BTreeMap::new()),
            next_blocked_listener: std::sync::atomic::AtomicU64::new(1),
            edges: Mutex::new(BTreeMap::new()),
            conversation_owners: Mutex::new(BTreeSet::new()),
            settled: Mutex::new(BTreeMap::new()),
            fail_fast_checks: Mutex::new(BTreeSet::new()),
            cascade_pending: AtomicBool::new(false),
            reconcile_scheduled: AtomicBool::new(false),
            reconciling: AtomicBool::new(false),
            wake_scheduled: AtomicBool::new(false),
            unsubscribe_registry: Mutex::new(None),
            enabled: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            draining: AtomicBool::new(false),
        }
    }

    async fn resolve_agent(
        self: &Arc<Self>,
        id: ConversationId,
        snapshot: Arc<dyn RegistrySnapshot>,
        context: &Context,
    ) -> SessionResult<Agent> {
        use crate::harness::agent::{AGENT_DOC, resolve_agent};
        let value = self
            .session
            .snapshot(&AGENT_DOC, DocOwner::Conversation(id), context)
            .await?;
        let state: Option<AgentState> = match value {
            Some(value) => Some(
                serde_json::from_value(JsonValue::Object(value))
                    .map_err(|error| SessionError::message(error.to_string()))?,
            ),
            None => None,
        };
        // Wrapper renames report `extension_error`; route them through the
        // scheduler's telemetry path like every other scheduler report.
        let report_info: crate::harness::types::ReportInfoFn = {
            let inner = Arc::clone(self);
            let context = context.clone();
            Arc::new(move |info| inner.report_fixed_with(info, Some(&context)))
        };
        Ok(resolve_agent(
            state.as_ref(),
            snapshot.as_ref(),
            &self.settings(),
            &self.report,
            Some(&report_info),
        ))
    }

    async fn build_env(
        self: &Arc<Self>,
        id: ConversationId,
        context: &Context,
    ) -> SessionResult<Option<Arc<dyn ExecutionEnv>>> {
        use crate::harness::agent::AGENT_DOC;
        let Some(build) = &self.env else {
            return Ok(None);
        };
        let cwd = self
            .session
            .snapshot(&AGENT_DOC, DocOwner::Conversation(id), context)
            .await?
            .and_then(|value| {
                value
                    .get("cwd")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
            });
        let read: Arc<dyn DocumentReader> = Arc::new(SchedulerDocumentReader {
            session: Arc::clone(&self.session),
        });
        Ok(build(
            EnvTarget {
                conversation_id: id,
                cwd,
                read,
            },
            context.clone(),
        )
        .await)
    }

    // ─── Lifecycle ──────────────────────────────────────────────────────────

    async fn open(self: &Arc<Self>, context: &Context) -> SessionResult<()> {
        let observer = Arc::clone(self);
        self.session
            .subscribe_internal_commits(Arc::new(move |publication, context| {
                observer.observe(publication, context);
            }));
        let sealer = Arc::clone(self);
        self.session
            .subscribe_close(Arc::new(move || sealer.seal()));
        let kicker = Arc::clone(self);
        let subscription = self.registry.subscribe(Arc::new(move || {
            kicker.notify_blocked();
            kicker.kick();
        }));
        if let Ok(mut slot) = self.unsubscribe_registry.lock() {
            *slot = Some(subscription);
        }
        let this = Arc::clone(self);
        self.session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    Box::pin(async move { this.open_tasks(tx).await })
                }),
                context,
                None,
            )
            .await?;
        // Derive abort marks a crash left unapplied below cancelled owners, and
        // finalize held outcomes.
        self.cascade_pending.store(true, Ordering::SeqCst);
        self.schedule_reconcile();
        Ok(())
    }

    async fn open_tasks(self: &Arc<Self>, tx: &mut Transaction) -> SessionResult<()> {
        // Every table read before the first write.
        let mut scans: Vec<Vec<TaskRecord>> = Vec::new();
        let statuses = [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Waiting,
            TaskStatus::Sleeping,
            TaskStatus::Completing,
        ];
        for status in statuses {
            let mut records = Vec::new();
            let mut cursor = None;
            loop {
                let page = tx
                    .scan_tasks(
                        TaskQuery {
                            status: Some(status),
                            ..TaskQuery::default()
                        },
                        SCAN_PAGE_SIZE,
                        cursor,
                    )
                    .await?;
                records.extend(page.items);
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            scans.push(records);
        }
        let mut parked: Vec<TaskRecord> = Vec::new();
        for records in scans {
            for record in records {
                let mut record = record;
                if let Ok(mut resumed) = self.resumed.lock() {
                    resumed.insert(record.id);
                }
                // A task its creator awaited only in memory is abandoned after a
                // restart: it is marked below, after the crash count, so that the
                // mark's fresh count is the one the record ends with (upstream
                // counts first and marks in the first reservation pass).
                let abandoned = record.abandon_on_restart && !record.abort_requested;
                if let TaskState::Running { checkpoint } = &record.state {
                    // A crash, eviction, or reset cut the phase off: count it and
                    // write the task back as pending. A normal close writes the
                    // same state in `join` without counting.
                    record.restarts = Some(record.restarts.unwrap_or(0) + 1);
                    let mut pending = with_state(
                        &record,
                        TaskState::Pending {
                            checkpoint: checkpoint.clone(),
                        },
                    );
                    pending.restarts = record.restarts;
                    if let Some(limit) = self.restart_limit().limit()
                        && record.restarts.unwrap_or(0) as u64 >= limit
                        && !record.abort_requested
                    {
                        parked.push(record.clone());
                    }
                    tx.set_task(pending.clone())?;
                    record = pending;
                }
                // Mark it `restart` so its abort handler runs instead of a phase,
                // and let the cascade reach its owned work. The mark starts the
                // restart count over: it now counts cut-offs of the handler.
                if abandoned {
                    record = with_abort_mark(&record, Some("restart"));
                    tx.set_task(record.clone())?;
                    self.cascade_pending.store(true, Ordering::SeqCst);
                }
                if let Ok(mut live) = self.live.lock() {
                    live.insert(record.id, record.clone());
                }
                self.telemetry.record_metric(
                    "pi.durable.tasks.active",
                    1.0,
                    serde_json::json!({ "task": record.kind })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                );
                if let TaskState::Waiting {
                    policy: JoinPolicy::FailFast,
                    ..
                } = &record.state
                    && let Ok(mut set) = self.fail_fast_checks.lock()
                {
                    set.insert(record.id);
                }
            }
        }
        for record in parked {
            let restarts = record.restarts.unwrap_or(0);
            (self.report)(format!(
                "Task {} ({}) was cut off {restarts} times without progress; it is parked until retryTask()",
                record.id, record.kind
            ));
            self.report_fixed(
                crate::harness::types::ReportInfo::new(
                    crate::harness::types::ReportKind::TaskParked,
                )
                .with_task(record.id, record.kind.clone()),
            );
        }
        Ok(())
    }

    /// The Session's failure cause, when it failed.
    fn failure(&self) -> Option<String> {
        self.session.failure()
    }

    /// The resolved restart limit.
    fn settings(&self) -> Settings {
        crate::harness::agent::resolve_settings(self.settings.current().as_ref())
    }

    fn restart_limit(&self) -> crate::harness::types::RestartLimit {
        self.settings().restart_limit
    }

    /// Whether a subject was parked by the restart limit.
    fn parked_subject(&self, record: &BlockedSubject) -> bool {
        !record.abort_requested
            && self
                .restart_limit()
                .limit()
                .map(|limit| record.restarts.unwrap_or(0) as u64 >= limit)
                .unwrap_or(false)
    }

    /// Why a live task cannot run here, if it cannot (upstream `blockedReason`).
    fn blocked_reason(&self, record: &BlockedSubject) -> Option<BlockedReason> {
        match record.status {
            TaskStatus::Running | TaskStatus::Completing | TaskStatus::Terminal => return None,
            _ => {}
        }
        // An abort request runs its handler or orphans the task; a `restart`
        // mark waits for the definition, blocked.
        if record.abort_requested && record.abort_reason.is_none() {
            return None;
        }
        if self.parked_subject(record) {
            return Some(BlockedReason::RestartLimit);
        }
        let snapshot = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.registry.snapshot()
        })) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                if !self.registry_snapshot_reported.swap(true, Ordering::SeqCst) {
                    (self.report)("The task registry snapshot failed".to_string());
                    self.report_fixed(
                        crate::harness::types::ReportInfo::new(
                            crate::harness::types::ReportKind::HostError,
                        )
                        .with_callback("registry"),
                    );
                }
                return None;
            }
        };
        let task = snapshot.task(&record.kind);
        match self.fit_subject(record.id, record.version, task.clone()) {
            Fit::Blocked { reason, .. } => Some(reason),
            Fit::Ready { migrates, .. } => {
                if migrates && task.is_some_and(|task| !task.has_migration()) {
                    Some(BlockedReason::MigrationFailed)
                } else {
                    None
                }
            }
        }
    }

    /// Call every task-graph listener; one that panics does not stop the others.
    fn notify_blocked(&self) {
        let listeners: Vec<BlockedListener> = self
            .blocked_listeners
            .lock()
            .map(|listeners| listeners.values().cloned().collect())
            .unwrap_or_default();
        for listener in listeners {
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener()))
            {
                let message = crate::session::session::panic_message(payload);
                (self.report)(format!("A blocked listener failed: {message}"));
                self.report_fixed(crate::harness::types::ReportInfo::new(
                    crate::harness::types::ReportKind::ListenerError,
                ));
            }
        }
    }

    fn report_fixed(&self, info: crate::harness::types::ReportInfo) {
        self.report_fixed_with(info, None);
    }

    fn report_fixed_with(
        &self,
        info: crate::harness::types::ReportInfo,
        context: Option<&Context>,
    ) {
        let telemetry = context
            .map(Context::telemetry)
            .unwrap_or_else(|| self.telemetry.clone());
        emit_report_telemetry(&telemetry, &info);
        if let Some(report) = &self.report_info {
            // A host callback that throws is contained (upstream `contained`).
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| report(info)));
        }
    }

    /// Whether a record is parked by the restart limit.
    fn parked(&self, record: &TaskRecord) -> bool {
        self.parked_subject(&BlockedSubject::of(record))
    }

    fn resume(self: &Arc<Self>) {
        self.enabled.store(true, Ordering::SeqCst);
        self.schedule_wake();
        self.kick();
    }

    /// When this Session next needs a running process.
    fn wake_snapshot(self: &Arc<Self>) -> Wake {
        let now = (self.now)();
        // A reconcile pass with work to do is work as well: it finalizes held
        // outcomes and marks cascades. Counting the scheduled/running pass
        // keeps a host from hibernating between the commit that created that
        // work and the pass that lands it.
        let mut active = self
            .invocations
            .lock()
            .map(|map| !map.is_empty())
            .unwrap_or(false)
            || self.reconcile_scheduled.load(Ordering::SeqCst)
            || self.reconciling.load(Ordering::SeqCst)
            || self.cascade_pending.load(Ordering::SeqCst)
            || self
                .fail_fast_checks
                .lock()
                .map(|checks| !checks.is_empty())
                .unwrap_or(false);
        let mut at: Option<f64> = None;
        let mut snapshot: Option<Arc<dyn RegistrySnapshot>> = None;
        let mut snapshot_failed = false;
        let records: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        for record in records {
            if matches!(record.state, TaskState::Completing { .. }) {
                continue;
            }
            if let TaskState::Waiting { on, .. } = &record.state
                && !record.abort_requested
                && on.iter().any(|id| {
                    self.live
                        .lock()
                        .map(|live| live.contains_key(id))
                        .unwrap_or(false)
                })
            {
                continue;
            }
            if let TaskState::Sleeping { until, .. } = &record.state
                && !record.abort_requested
                && *until > now
            {
                at = Some(at.map_or(*until, |current| current.min(*until)));
                continue;
            }
            if active {
                continue;
            }
            // A parked task waits for `retryTask()`, not for a process.
            if self.parked(&record) {
                continue;
            }
            if record.abort_requested
                && !self.waiting_on(&record, &self.owned_live(None)).is_empty()
            {
                continue;
            }
            if !record.abort_requested || record.abort_reason.is_some() {
                if snapshot.is_none() && !snapshot_failed {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.registry.snapshot()
                    })) {
                        Ok(value) => snapshot = Some(value),
                        Err(_) => {
                            snapshot_failed = true;
                            if !self.registry_snapshot_reported.swap(true, Ordering::SeqCst) {
                                (self.report)("The task registry snapshot failed".to_string());
                                self.report_fixed(
                                    crate::harness::types::ReportInfo::new(
                                        crate::harness::types::ReportKind::HostError,
                                    )
                                    .with_callback("registry"),
                                );
                            }
                        }
                    }
                }
                // A failed snapshot says nothing about this task; keep the
                // host awake (upstream also treats it as runnable).
                if let Some(snapshot) = &snapshot
                    && matches!(
                        self.fit(&record, snapshot.task(&record.kind)),
                        Fit::Blocked { .. }
                    )
                {
                    continue;
                }
            }
            active = true;
        }
        Wake { active, at }
    }

    /// Deliver the current wake value to the host, or arm the earliest sleep.
    fn schedule_wake(self: &Arc<Self>) {
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        if let Some(on_wake) = self.on_wake.clone() {
            // Like upstream's microtask: never call the host synchronously from
            // a commit listener, where a host callback that reads or commits
            // would deadlock on the Session line.
            if self.wake_scheduled.swap(true, Ordering::SeqCst) {
                return;
            }
            let this = Arc::clone(self);
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    this.wake_scheduled.store(false, Ordering::SeqCst);
                    this.deliver_wake(&on_wake);
                });
            } else {
                self.wake_scheduled.store(false, Ordering::SeqCst);
                self.deliver_wake(&on_wake);
            }
            return;
        }
        let wake = self.wake_snapshot();
        self.arm_wake_timer(if self.enabled.load(Ordering::SeqCst) {
            wake.at
        } else {
            None
        });
    }

    /// Hand one wake value to `onWake`, deduplicating an unchanged one.
    fn deliver_wake(self: &Arc<Self>, on_wake: &WakeFn) {
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        let wake = self.wake_snapshot();
        let same = self
            .last_wake
            .lock()
            .map(|last| {
                last.map(|last| last.active == wake.active && last.at == wake.at)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if same {
            return;
        }
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_wake(wake))).is_ok() {
            if let Ok(mut last) = self.last_wake.lock() {
                *last = Some(wake);
            }
        } else {
            (self.report)("HarnessOptions.onWake threw".to_string());
        }
    }

    fn arm_wake_timer(self: &Arc<Self>, at: Option<f64>) {
        let mut slot = match self.wake_timer.lock() {
            Ok(slot) => slot,
            Err(_) => return,
        };
        if slot
            .as_ref()
            .map(|(current, _)| Some(*current) == at)
            .unwrap_or(at.is_none())
        {
            return;
        }
        if let Some((_, disarm)) = slot.take() {
            disarm();
        }
        let Some(at) = at else {
            return;
        };
        let this = Arc::clone(self);
        let callback = Arc::new(move || {
            if let Ok(mut slot) = this.wake_timer.lock() {
                slot.take();
            }
            // Recheck the clock in the scheduling pass: an early wake arms again.
            this.schedule_wake();
            this.kick();
        });
        let delay = (at - (self.now)()).max(0.0);
        let disarm = self.timers.foreground(callback, delay);
        *slot = Some((at, disarm));
    }

    async fn retry_task(self: &Arc<Self>, id: TaskId, context: &Context) -> SessionResult<()> {
        let this = Arc::clone(self);
        self.session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    Box::pin(async move {
                        let Some(current) = tx.task(id).await? else {
                            return Err(SessionError::message(format!("Task {id} does not exist")));
                        };
                        if matches!(
                            current.state,
                            TaskState::Terminal { .. } | TaskState::Completing { .. }
                        ) || !this.parked(&current)
                        {
                            return Ok(());
                        }
                        let mut next = current.clone();
                        next.restarts = None;
                        tx.set_task(next)?;
                        Ok(())
                    })
                }),
                context,
                None,
            )
            .await?;
        self.resume();
        Ok(())
    }

    async fn join(self: &Arc<Self>) -> SessionResult<()> {
        // A close is not a crash: mark the scheduler closing before cancelling
        // invocations, so a phase ended by the close is `aborted`, not faulted.
        self.closing.store(true, Ordering::SeqCst);
        self.disarm_timers();
        let invocations: Vec<Arc<Invocation>> = self
            .invocations
            .lock()
            .map(|map| map.values().cloned().collect())
            .unwrap_or_default();
        for invocation in &invocations {
            invocation.controller.cancel();
            wait_invocation(invocation).await;
        }
        if invocations.is_empty() {
            return Ok(());
        }
        // A close is not a crash: write what the stopped invocations ran back as
        // pending without counting a restart.
        let ids: Vec<TaskId> = invocations
            .iter()
            .map(|invocation| invocation.task_id)
            .collect();
        let this = Arc::clone(self);
        let result = self
            .session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    let ids = ids.clone();
                    Box::pin(async move {
                        for id in ids {
                            let record = this
                                .live
                                .lock()
                                .ok()
                                .and_then(|live| live.get(&id).cloned());
                            if let Some(record) = record
                                && let TaskState::Running { checkpoint } = &record.state
                            {
                                // A close does not count a restart, but it keeps
                                // the count a crash already accumulated.
                                let next = with_state(
                                    &record,
                                    TaskState::Pending {
                                        checkpoint: checkpoint.clone(),
                                    },
                                );
                                tx.set_task(next)?;
                            }
                        }
                        Ok(())
                    })
                }),
                &self.context,
                None,
            )
            .await;
        result?;
        Ok(())
    }

    async fn abort(
        self: &Arc<Self>,
        id: TaskId,
        context: &Context,
        keep_restart: bool,
    ) -> SessionResult<TaskAbort> {
        let this = Arc::clone(self);
        let outcome = self
            .session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    Box::pin(async move { this.abort_in(tx, id, keep_restart).await })
                }),
                context,
                None,
            )
            .await?;
        if let Some(run) = &outcome.run {
            // The commit listener signalled the run; join it, unless the Session
            // fails first: a run that ignores its signal must not outlive it
            // (upstream races the join against the Session failure).
            let invocation = Arc::clone(run);
            let joined = await_with_context(wait_invocation(&invocation), context);
            tokio::select! {
                result = joined => {
                    if result.is_err() {
                        return Err(SessionError::Aborted);
                    }
                }
                _ = self.session.closed() => {}
            }
        }
        Ok(outcome.result)
    }

    async fn abort_in(
        self: &Arc<Self>,
        tx: &mut Transaction,
        id: TaskId,
        keep_restart: bool,
    ) -> SessionResult<AbortCommit> {
        let Some(current) = tx.task(id).await? else {
            return Err(SessionError::message(format!("Task {id} does not exist")));
        };
        if matches!(current.state, TaskState::Terminal { .. }) {
            return Ok(AbortCommit {
                result: TaskAbort::Terminal,
                run: None,
            });
        }
        let invocation = self
            .invocations
            .lock()
            .ok()
            .and_then(|map| map.get(&id).cloned());
        // An owner's own cleanup keeps a restart mark: the task waits for its
        // definition and is not orphaned (upstream `keepRestart`).
        if keep_restart && current.abort_reason.as_deref() == Some("restart") {
            return Ok(AbortCommit {
                result: TaskAbort::Marked,
                run: None,
            });
        }
        if invocation.is_none() && !matches!(current.state, TaskState::Completing { .. }) {
            self.load_scopes(false).await?;
            if !self.owned_live(None).contains_key(&id) {
                let snapshot = self.registry.snapshot();
                if let Resolution::Blocked { reason, .. } = self.resolve(&current, &snapshot) {
                    self.terminate(
                        tx,
                        &current,
                        TaskOutcome::Orphaned {
                            reason: blocked_reason_str(reason).to_string(),
                        },
                    )
                    .await?;
                    return Ok(AbortCommit {
                        result: TaskAbort::Marked,
                        run: None,
                    });
                }
            }
        }
        // A request replaces a restart mark; a plain request already written is
        // left alone, and so is a terminal or completing task's own cleanup.
        if !current.abort_requested || current.abort_reason.is_some() {
            tx.set_task(with_abort_mark(&current, None))?;
        }
        let run = invocation.filter(|invocation| invocation.mode == InvocationMode::Run);
        Ok(AbortCommit {
            result: TaskAbort::Marked,
            run,
        })
    }

    async fn wait_for_task(
        self: &Arc<Self>,
        id: TaskId,
        context: &Context,
    ) -> SessionResult<SettledTask<JsonValue>> {
        let this = Arc::clone(self);
        let read_context = context.clone();
        let found: SessionResult<WaitFuture<SettledTask<JsonValue>>> = self
            .session
            .read_on_line(move || {
                let this = Arc::clone(&this);
                let read_context = read_context.clone();
                Box::pin(async move {
                    if let Some(cause) = this.failure() {
                        return Err(SessionError::Failed { cause });
                    }
                    if this.closing.load(Ordering::SeqCst) {
                        return Err(SessionError::Closed);
                    }
                    if this
                        .live
                        .lock()
                        .map(|live| live.contains_key(&id))
                        .unwrap_or(false)
                    {
                        return Ok(this.task_waiters.add(id, &read_context));
                    }
                    let record = this.storage.task(id, &read_context).await?;
                    match record {
                        None => Err(SessionError::message(format!("Task {id} does not exist"))),
                        Some(record) => Ok(Box::pin(async move { Ok(settled_task(record)) })
                            as WaitFuture<SettledTask<JsonValue>>),
                    }
                })
            })
            .await;
        found?.await.map_err(Into::into)
    }

    async fn wait_for_idle(
        self: &Arc<Self>,
        conversation_id: Option<ConversationId>,
        context: &Context,
    ) -> SessionResult<()> {
        if let Some(cause) = self.failure() {
            return Err(SessionError::Failed { cause });
        }
        if self.closing.load(Ordering::SeqCst) {
            return Err(SessionError::Closed);
        }
        if self.idle(conversation_id) {
            return Ok(());
        }
        self.schedule_reconcile();
        self.idle_waiters
            .add(conversation_id, context)
            .await
            .map_err(Into::into)
    }

    async fn abort_conversation(
        self: &Arc<Self>,
        conversation_id: ConversationId,
        background: bool,
        context: &Context,
    ) -> SessionResult<()> {
        let this = Arc::clone(self);
        let reached = self
            .session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    Box::pin(async move {
                        let queued = this.load_scopes(true).await?;
                        let scope = Scope::Conversation(conversation_id);
                        let mut reached: Vec<TaskId> = Vec::new();
                        let records: Vec<TaskRecord> = this
                            .live
                            .lock()
                            .map(|live| live.values().cloned().collect())
                            .unwrap_or_default();
                        for record in records {
                            if record.background && !background {
                                continue;
                            }
                            if this.in_scope(parent_of(&record), &scope, background) != Some(true) {
                                continue;
                            }
                            reached.push(record.id);
                            if !record.abort_requested || record.abort_reason.is_some() {
                                tx.set_task(with_abort_mark(&record, None))?;
                            }
                        }
                        for id in queued {
                            if this.in_scope(Up::Conversation(id), &scope, background) == Some(true)
                            {
                                crate::harness::inbox::withdraw_queued_inputs(tx, id).await?;
                            }
                        }
                        Ok(reached)
                    })
                }),
                context,
                None,
            )
            .await?;
        if background {
            for id in reached {
                let _ = self.wait_for_task(id, context).await;
            }
        }
        self.wait_for_idle(Some(conversation_id), context).await
    }

    /// Fail the Session for one of the scheduler's own commits; ignored once
    /// closing. The Session reports the failure once.
    fn fail_session(&self, error: impl std::fmt::Display) {
        if !self.closing.load(Ordering::SeqCst) {
            self.session.fail(error.to_string());
        }
    }

    /// Disarm the earliest-sleep timer and the idle-context expiry timer.
    fn disarm_timers(&self) {
        if let Ok(mut slot) = self.wake_timer.lock()
            && let Some((_, disarm)) = slot.take()
        {
            disarm();
        }
        if let Ok(mut slot) = self.expiry_timer.lock()
            && let Some((_, disarm)) = slot.take()
        {
            disarm();
        }
    }

    fn seal(self: &Arc<Self>) {
        self.closing.store(true, Ordering::SeqCst);
        self.disarm_timers();
        if let Ok(mut slot) = self.unsubscribe_registry.lock()
            && let Some(subscription) = slot.take()
        {
            subscription.unsubscribe();
        }
        // A failed Session rejects its remaining waits with the cause; only a
        // normal close rejects as closed (upstream `closedError(session)`).
        let error = match self.failure() {
            Some(cause) => crate::harness::util::WaitError::Failed(cause),
            None => closed_error(),
        };
        self.task_waiters.reject_all(error.clone());
        self.idle_waiters.reject_all(error);
        if let Ok(mut contexts) = self.contexts.lock() {
            contexts.clear();
        }
        if let Ok(mut resumed) = self.resumed.lock() {
            resumed.clear();
        }
        let invocations: Vec<Arc<Invocation>> = self
            .invocations
            .lock()
            .map(|map| map.values().cloned().collect())
            .unwrap_or_default();
        for invocation in invocations {
            invocation.controller.cancel();
        }
    }

    // ─── Observation ────────────────────────────────────────────────────────

    fn observe(self: &Arc<Self>, publication: &CommitPublication, context: &Context) {
        let telemetry = context.telemetry();
        let mut updated: Vec<TaskRecord> = Vec::new();
        let mut failed: Vec<TaskId> = Vec::new();
        let mut changed = false;
        for change in &publication.changes {
            match change {
                CommitChange::Table(StorageWrite::Entry { value }) => {
                    telemetry.add_event(
                        "pi.durable.entry",
                        serde_json::json!({
                            "entry.id": value.id.get(),
                            "entry.kind": value.kind,
                        })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    );
                    continue;
                }
                CommitChange::Table(StorageWrite::Submission { value })
                    if crate::harness::submissions::is_settled(value) =>
                {
                    let mut attributes = serde_json::json!({
                        "submission.id": value.id().get(),
                        "status": value.status(),
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                    if let SubmissionRecord::Input {
                        status: crate::types::InputSubmissionStatus::Done { answer, .. },
                        ..
                    } = value
                    {
                        attributes
                            .insert("answer.entry_id".to_string(), JsonValue::from(answer.get()));
                    }
                    telemetry.add_event("pi.durable.submission", attributes);
                    continue;
                }
                _ => {}
            }
            let CommitChange::Table(StorageWrite::Task { value: record }) = change else {
                continue;
            };
            changed = true;
            let previous = self
                .live
                .lock()
                .ok()
                .and_then(|live| live.get(&record.id).cloned());
            let previous_failed = previous.as_ref().map(failed_outcome).unwrap_or(false);
            if failed_outcome(record) && !previous_failed {
                failed.push(record.id);
            }
            if matches!(record.state, TaskState::Terminal { .. }) {
                if previous.is_some() {
                    telemetry.record_metric(
                        "pi.durable.tasks.active",
                        -1.0,
                        serde_json::json!({ "task": record.kind })
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    );
                }
                if let Ok(mut live) = self.live.lock() {
                    live.remove(&record.id);
                }
                if let Ok(mut map) = self.failed_migrations.lock() {
                    map.remove(&record.id);
                }
                if let Ok(mut set) = self.fail_fast_checks.lock() {
                    set.remove(&record.id);
                }
                if let Ok(mut resumed) = self.resumed.lock() {
                    resumed.remove(&record.id);
                }
                let is_owner = self
                    .conversation_owners
                    .lock()
                    .map(|owners| owners.contains(&record.id))
                    .unwrap_or(false);
                if is_owner && let Ok(mut settled) = self.settled.lock() {
                    settled.insert(record.id, node_of(record));
                }
                self.task_waiters
                    .resolve(record.id, settled_task(record.clone()));
                // Its owner may finalize now.
                self.schedule_reconcile();
                continue;
            }
            let was_marked = previous
                .as_ref()
                .map(|previous| previous.abort_requested)
                .unwrap_or(false);
            if record.abort_requested && !was_marked {
                self.cascade_pending.store(true, Ordering::SeqCst);
                if let Some(invocation) = self
                    .invocations
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&record.id).cloned())
                    && invocation.mode == InvocationMode::Run
                {
                    invocation.controller.cancel();
                }
            }
            // A request replacing a `restart` mark upgrades the marks of the work
            // below it (upstream `#apply`).
            let upgraded = previous
                .as_ref()
                .is_some_and(|previous| previous.abort_reason.is_some())
                && record.abort_reason.is_none();
            if upgraded {
                self.cascade_pending.store(true, Ordering::SeqCst);
            }
            let was_completing = previous
                .as_ref()
                .map(|previous| matches!(previous.state, TaskState::Completing { .. }))
                .unwrap_or(false);
            if matches!(record.state, TaskState::Completing { .. }) && !was_completing {
                if cancellation_intent(record) {
                    self.cascade_pending.store(true, Ordering::SeqCst);
                }
                self.schedule_reconcile();
            }
            let was_waiting = previous
                .as_ref()
                .map(|previous| matches!(previous.state, TaskState::Waiting { .. }))
                .unwrap_or(false);
            if let TaskState::Waiting {
                policy: JoinPolicy::FailFast,
                ..
            } = &record.state
                && !was_waiting
            {
                if let Ok(mut set) = self.fail_fast_checks.lock() {
                    set.insert(record.id);
                }
                self.schedule_reconcile();
            }
            if let Ok(mut live) = self.live.lock() {
                live.insert(record.id, record.clone());
            }
            if previous.is_none() {
                telemetry.record_metric(
                    "pi.durable.tasks.active",
                    1.0,
                    serde_json::json!({ "task": record.kind })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            updated.push(record.clone());
        }
        for id in failed {
            let records: Vec<TaskRecord> = self
                .live
                .lock()
                .map(|live| live.values().cloned().collect())
                .unwrap_or_default();
            for record in records {
                if let TaskState::Waiting {
                    on,
                    policy: JoinPolicy::FailFast,
                    ..
                } = &record.state
                    && on.contains(&id)
                {
                    if let Ok(mut set) = self.fail_fast_checks.lock() {
                        set.insert(record.id);
                    }
                    self.schedule_reconcile();
                }
            }
        }
        for change in &publication.changes {
            if let CommitChange::Table(StorageWrite::Conversation { value }) = change {
                let known = self
                    .edges
                    .lock()
                    .map(|edges| edges.contains_key(&value.id))
                    .unwrap_or(false);
                if !known {
                    self.set_edge(value.id, value.owner.as_ref().map(|owner| owner.task_id));
                }
            }
        }
        for change in &publication.changes {
            let CommitChange::Table(StorageWrite::Submission { value }) = change else {
                continue;
            };
            if value.status() != "queued" || !matches!(value, SubmissionRecord::Input { .. }) {
                continue;
            }
            let up = Up::Conversation(value.conversation_id());
            if !self.chain_known(up, None) || self.below_cancelled(up) {
                self.cascade_pending.store(true, Ordering::SeqCst);
            }
        }
        for record in &updated {
            if !self.chain_known(parent_of(record), None) {
                self.schedule_reconcile();
            } else if !record.background
                && !record.abort_requested
                && self.below_cancelled(parent_of(record))
            {
                self.cascade_pending.store(true, Ordering::SeqCst);
            }
        }
        if self.cascade_pending.load(Ordering::SeqCst) {
            self.schedule_reconcile();
        }
        if !changed {
            return;
        }
        self.prune_ownership_indexes();
        self.settle_idle();
        self.kick();
    }

    /// Keep only the terminal ownership nodes a live task's walk still reaches.
    fn prune_ownership_indexes(self: &Arc<Self>) {
        let records: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        let mut needed: BTreeSet<TaskId> = BTreeSet::new();
        for record in &records {
            for step in self.above(parent_of(record), None) {
                if let Step::Task(id, _) = step {
                    needed.insert(id);
                }
            }
        }
        if let Ok(mut settled) = self.settled.lock() {
            settled.retain(|id, _| needed.contains(id));
        }
        if let Ok(mut owners) = self.conversation_owners.lock() {
            owners.retain(|id| needed.contains(id));
        }
    }

    fn settle_idle(self: &Arc<Self>) {
        for conversation_id in self.idle_waiters.keys() {
            if self.idle(conversation_id) {
                self.idle_waiters.resolve(conversation_id, ());
            }
        }
        let now = (self.now)();
        let retention = self.settings().context_retention_ms as f64;
        let Ok(mut contexts) = self.contexts.lock() else {
            return;
        };
        let keys: Vec<ConversationId> = contexts.keys().copied().collect();
        for id in keys {
            let expired = contexts
                .get(&id)
                .and_then(|kept| kept.idle_since)
                .map(|since| now - since >= retention)
                .unwrap_or(false);
            if expired {
                contexts.remove(&id);
                continue;
            }
            if !self.idle(Some(id)) {
                if let Some(kept) = contexts.get_mut(&id) {
                    kept.idle_since = None;
                }
            } else if retention > 0.0 {
                if let Some(kept) = contexts.get_mut(&id)
                    && kept.idle_since.is_none()
                {
                    kept.idle_since = Some(now);
                }
            } else {
                contexts.remove(&id);
            }
        }
        drop(contexts);
        self.schedule_expiry();
    }

    // ─── Ownership ──────────────────────────────────────────────────────────

    fn schedule_reconcile(self: &Arc<Self>) {
        if self.reconcile_scheduled.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            return;
        }
        self.reconcile_scheduled.store(true, Ordering::SeqCst);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.reconcile().await;
        });
    }

    async fn reconcile(self: &Arc<Self>) {
        self.reconciling.store(true, Ordering::SeqCst);
        let _reconciling = ReconcilingGuard(&self.reconciling);
        self.reconcile_scheduled.store(false, Ordering::SeqCst);
        let cascade = self.cascade_pending.swap(false, Ordering::SeqCst);
        let checks: Vec<TaskId> = self
            .fail_fast_checks
            .lock()
            .map(|mut set| std::mem::take(&mut *set).into_iter().collect())
            .unwrap_or_default();
        let this = Arc::clone(self);
        let checks_for_commit = checks.clone();
        let result = self
            .session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    let checks = checks_for_commit.clone();
                    Box::pin(async move { this.reconcile_in(tx, cascade, checks).await })
                }),
                &self.context,
                None,
            )
            .await;
        if let Err(error) = result {
            // Any pass may have staged marks, so a failed one is retried next commit.
            self.cascade_pending.store(true, Ordering::SeqCst);
            if let Ok(mut set) = self.fail_fast_checks.lock() {
                for id in checks {
                    set.insert(id);
                }
            }
            self.fail_session(error);
        }
        self.settle_idle();
    }

    async fn reconcile_in(
        self: &Arc<Self>,
        tx: &mut Transaction,
        cascade: bool,
        checks: Vec<TaskId>,
    ) -> SessionResult<()> {
        if self.closing.load(Ordering::SeqCst) {
            return Ok(());
        }
        let queued = self.load_scopes(cascade).await?;
        // A cascade from an abandoned owner passes its `restart` reason on; any
        // other intent marks, or upgrades a `restart` mark, as a request.
        // Staged first, so a request wins over a `restart` mark of the same pass
        // (upstream `#reconcile` `mark()`).
        let mut marks: BTreeMap<TaskId, (TaskRecord, Option<String>)> = BTreeMap::new();
        let records: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        for record in records {
            if record.background {
                continue;
            }
            // The nearest live owner with cancellation intent passes its reason
            // down: only one abandoned after a restart, and nothing else.
            let Some(owner) = self.below_cancelled_owner(parent_of(&record)) else {
                continue;
            };
            let restart =
                owner.abort_reason.as_deref() == Some("restart") && !failed_outcome(&owner);
            stage_mark(&mut marks, &record, restart.then_some("restart"));
        }
        for id in checks {
            let waiter = self
                .live
                .lock()
                .ok()
                .and_then(|live| live.get(&id).cloned());
            let Some(waiter) = waiter else {
                continue;
            };
            let TaskState::Waiting { on, .. } = &waiter.state else {
                continue;
            };
            if !self.any_failed(on).await? {
                continue;
            }
            // Every other live task: the failed one keeps its own outcome.
            let on = on.clone();
            for member in on {
                let member = self
                    .live
                    .lock()
                    .ok()
                    .and_then(|live| live.get(&member).cloned());
                if let Some(record) = member
                    && !failed_outcome(&record)
                {
                    stage_mark(&mut marks, &record, None);
                }
            }
        }
        for (_, (record, reason)) in marks {
            tx.set_task(with_abort_mark(&record, reason.as_deref()))?;
        }
        for id in queued {
            if self.below_cancelled(Up::Conversation(id)) {
                crate::harness::inbox::withdraw_queued_inputs(tx, id).await?;
            }
        }
        self.finalize(tx).await?;
        Ok(())
    }

    async fn any_failed(self: &Arc<Self>, ids: &[TaskId]) -> SessionResult<bool> {
        for id in ids {
            let record = self.live.lock().ok().and_then(|live| live.get(id).cloned());
            let record = match record {
                Some(record) => Some(record),
                None => self.storage.task(*id, &self.context).await?,
            };
            if record
                .map(|record| failed_outcome(&record))
                .unwrap_or(false)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn finalize(self: &Arc<Self>, tx: &mut Transaction) -> SessionResult<()> {
        loop {
            let overlay = overlay_of(tx);
            let owned = self.owned_live(Some(&overlay));
            let done: Vec<TaskRecord> = self
                .live_records(Some(&overlay))
                .into_iter()
                .filter(|record| {
                    matches!(record.state, TaskState::Completing { .. })
                        && !owned.contains_key(&record.id)
                })
                .collect();
            if done.is_empty() {
                return Ok(());
            }
            for record in done {
                let TaskState::Completing { outcome } = &record.state else {
                    continue;
                };
                tx.set_task(with_state(
                    &record,
                    TaskState::Terminal {
                        outcome: outcome.clone(),
                    },
                ))?;
                // Only the scheduler writes `faulted` and `orphaned`; their
                // Harness cleanup waits for this commit.
                if let Some(scheduler_outcome) = scheduler_outcome(outcome) {
                    settle_scheduler_outcome(tx, &record, &scheduler_outcome).await?;
                }
            }
        }
    }

    async fn load_scopes(self: &Arc<Self>, queued: bool) -> SessionResult<Vec<ConversationId>> {
        let live: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        for record in live {
            if !self.chain_known(parent_of(&record), None) {
                self.load_chain(parent_of(&record), None).await?;
            }
        }
        if !queued {
            return Ok(Vec::new());
        }
        let mut conversations: BTreeSet<ConversationId> = BTreeSet::new();
        let mut cursor = None;
        loop {
            let page = self
                .storage
                .scan_submissions(
                    SubmissionQuery {
                        status: Some(SubmissionStatusFilter::Queued),
                        ..SubmissionQuery::default()
                    },
                    SCAN_PAGE_SIZE,
                    cursor,
                    &self.context,
                )
                .await?;
            for submission in page.items {
                conversations.insert(submission.conversation_id());
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        for id in &conversations {
            self.load_chain(Up::Conversation(*id), None).await?;
        }
        Ok(conversations.into_iter().collect())
    }

    async fn load_chain(
        self: &Arc<Self>,
        start: Up,
        overlay: Option<&Overlay>,
    ) -> SessionResult<()> {
        let mut at = Some(start);
        while let Some(current) = at {
            match current {
                Up::Task(task_id) => {
                    let node = match self.node(task_id, overlay) {
                        Some(node) => node,
                        None => {
                            let record = self.storage.task(task_id, &self.context).await?;
                            let Some(record) = record else {
                                return Ok(());
                            };
                            let node = node_of(&record);
                            if matches!(record.state, TaskState::Terminal { .. })
                                && let Ok(mut settled) = self.settled.lock()
                            {
                                settled.insert(record.id, node.clone());
                            }
                            node
                        }
                    };
                    at = Some(parent_of_node(&node));
                }
                Up::Conversation(id) => {
                    let edge = match self.edge(id, overlay) {
                        Some(edge) => edge,
                        None => {
                            let record = self.storage.conversation(id, &self.context).await?;
                            let owner =
                                record.and_then(|record| record.owner.map(|owner| owner.task_id));
                            self.set_edge(id, owner);
                            owner
                        }
                    };
                    at = edge.map(Up::Task);
                }
            }
        }
        Ok(())
    }

    fn set_edge(self: &Arc<Self>, conversation_id: ConversationId, owner: Option<TaskId>) {
        if let Ok(mut edges) = self.edges.lock() {
            edges.insert(conversation_id, owner);
        }
        if let Some(owner) = owner
            && let Ok(mut owners) = self.conversation_owners.lock()
        {
            owners.insert(owner);
        }
    }

    fn edge(
        self: &Arc<Self>,
        id: ConversationId,
        overlay: Option<&Overlay>,
    ) -> Option<Option<TaskId>> {
        if let Some(overlay) = overlay
            && let Some(edge) = overlay.edges.get(&id)
        {
            return Some(*edge);
        }
        self.edges
            .lock()
            .ok()
            .and_then(|edges| edges.get(&id).copied())
    }

    fn node(self: &Arc<Self>, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode> {
        if let Some(overlay) = overlay
            && let Some(record) = overlay.tasks.get(&id)
        {
            return Some(node_of(record));
        }
        if let Some(record) = self
            .live
            .lock()
            .ok()
            .and_then(|live| live.get(&id).cloned())
        {
            return Some(node_of(&record));
        }
        self.settled
            .lock()
            .ok()
            .and_then(|settled| settled.get(&id).cloned())
    }

    fn above(self: &Arc<Self>, start: Up, overlay: Option<&Overlay>) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut at = Some(start);
        while let Some(current) = at {
            match current {
                Up::Task(id) => {
                    let Some(node) = self.node(id, overlay) else {
                        steps.push(Step::Unknown);
                        return steps;
                    };
                    let parent = parent_of_node(&node);
                    steps.push(Step::Task(id, node));
                    at = Some(parent);
                }
                Up::Conversation(id) => {
                    steps.push(Step::Conversation(id));
                    let Some(edge) = self.edge(id, overlay) else {
                        steps.push(Step::Unknown);
                        return steps;
                    };
                    at = edge.map(Up::Task);
                }
            }
        }
        steps
    }

    fn chain_known(self: &Arc<Self>, start: Up, overlay: Option<&Overlay>) -> bool {
        !self
            .above(start, overlay)
            .iter()
            .any(|step| matches!(step, Step::Unknown))
    }

    fn live_records(self: &Arc<Self>, overlay: Option<&Overlay>) -> Vec<TaskRecord> {
        let mut records = Vec::new();
        let live: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        for record in live {
            let candidate = overlay
                .and_then(|overlay| overlay.tasks.get(&record.id))
                .cloned()
                .unwrap_or(record);
            if !matches!(candidate.state, TaskState::Terminal { .. }) {
                records.push(candidate);
            }
        }
        if let Some(overlay) = overlay {
            let known: BTreeSet<TaskId> = self
                .live
                .lock()
                .map(|live| live.keys().copied().collect())
                .unwrap_or_default();
            for record in overlay.tasks.values() {
                if !known.contains(&record.id)
                    && !matches!(record.state, TaskState::Terminal { .. })
                {
                    records.push(record.clone());
                }
            }
        }
        records
    }

    fn owned_live(self: &Arc<Self>, overlay: Option<&Overlay>) -> BTreeMap<TaskId, Vec<TaskId>> {
        let mut owned: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
        for record in self.live_records(overlay) {
            if record.background {
                continue;
            }
            for step in self.above(parent_of(&record), overlay) {
                match step {
                    Step::Unknown => break,
                    Step::Conversation(_) => continue,
                    Step::Task(id, node) => {
                        owned.entry(id).or_default().push(record.id);
                        if node.background {
                            break;
                        }
                    }
                }
            }
        }
        owned
    }

    fn in_scope(
        self: &Arc<Self>,
        start: Up,
        scope: &Scope,
        cross_background: bool,
    ) -> Option<bool> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(id) => {
                    if let Scope::Conversation(wanted) = scope
                        && id == *wanted
                    {
                        return Some(true);
                    }
                }
                Step::Task(_, node) => {
                    if node.background && !cross_background {
                        return Some(false);
                    }
                }
            }
        }
        Some(matches!(scope, Scope::Roots))
    }

    /// The nearest live owner above `start` with cancellation intent, if any
    /// (upstream `#belowCancelled`). Its `restart` mark is what a cascade passes on.
    fn below_cancelled_owner(self: &Arc<Self>, start: Up) -> Option<TaskRecord> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(_) => continue,
                Step::Task(id, node) => {
                    let live = self
                        .live
                        .lock()
                        .ok()
                        .and_then(|live| live.get(&id).cloned());
                    if live.as_ref().map(cancellation_intent).unwrap_or(false) {
                        return live;
                    }
                    if node.background {
                        return None;
                    }
                }
            }
        }
        None
    }

    fn below_cancelled(self: &Arc<Self>, start: Up) -> bool {
        self.below_cancelled_owner(start).is_some()
    }

    fn idle(self: &Arc<Self>, conversation_id: Option<ConversationId>) -> bool {
        let scope = match conversation_id {
            Some(id) => Scope::Conversation(id),
            None => Scope::Roots,
        };
        let records: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        for record in records {
            if !record.background && self.in_scope(parent_of(&record), &scope, false) != Some(false)
            {
                return false;
            }
        }
        true
    }

    /// Arm a background timer for the earliest idle-context expiry. A host
    /// that declines it leaves expiry to the next task change.
    fn schedule_expiry(self: &Arc<Self>) {
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        let retention = self.settings().context_retention_ms as f64;
        let at = self.contexts.lock().ok().and_then(|contexts| {
            contexts
                .values()
                .filter_map(|kept| kept.idle_since.map(|since| since + retention))
                .reduce(f64::min)
        });
        let mut slot = match self.expiry_timer.lock() {
            Ok(slot) => slot,
            Err(_) => return,
        };
        if slot
            .as_ref()
            .map(|(current, _)| Some(*current) == at)
            .unwrap_or(at.is_none())
        {
            return;
        }
        if let Some((_, disarm)) = slot.take() {
            disarm();
        }
        let Some(at) = at else {
            return;
        };
        let this = Arc::clone(self);
        let callback = Arc::new(move || {
            if let Ok(mut slot) = this.expiry_timer.lock() {
                slot.take();
            }
            this.settle_idle();
            this.schedule_expiry();
        });
        let delay = (at - (self.now)()).max(0.0);
        if let Some(disarm) = self.timers.background(callback, delay) {
            *slot = Some((at, disarm));
        }
    }

    // ─── Scheduling ─────────────────────────────────────────────────────────

    fn kick(self: &Arc<Self>) {
        self.schedule_wake();
        self.dirty.store(true, Ordering::SeqCst);
        if self.draining.load(Ordering::SeqCst)
            || !self.enabled.load(Ordering::SeqCst)
            || self.closing.load(Ordering::SeqCst)
        {
            return;
        }
        self.draining.store(true, Ordering::SeqCst);
        let this = Arc::clone(self);
        // Never commit synchronously from a commit or registry listener.
        tokio::spawn(async move {
            this.drain().await;
        });
    }

    async fn drain(self: &Arc<Self>) {
        let result: SessionResult<()> = async {
            while self.dirty.load(Ordering::SeqCst)
                && self.enabled.load(Ordering::SeqCst)
                && !self.closing.load(Ordering::SeqCst)
            {
                self.dirty.store(false, Ordering::SeqCst);
                let reservations = self.reserve().await?;
                for reservation in reservations {
                    self.start(&reservation);
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            self.fail_session(error);
        }
        self.draining.store(false, Ordering::SeqCst);
        // A wakeup that arrived during a failed pass still needs its pass.
        if self.dirty.load(Ordering::SeqCst) {
            self.kick();
        }
    }

    async fn reserve(self: &Arc<Self>) -> SessionResult<Vec<Reservation>> {
        let out: Arc<Mutex<Vec<Reservation>>> = Arc::new(Mutex::new(Vec::new()));
        let faulted: Arc<Mutex<Vec<(TaskRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let this = Arc::clone(self);
        let out_commit = Arc::clone(&out);
        let faulted_commit = Arc::clone(&faulted);
        let result = self
            .session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    let out = Arc::clone(&out_commit);
                    let faulted = Arc::clone(&faulted_commit);
                    Box::pin(async move { this.reserve_in(tx, &out, &faulted).await })
                }),
                &self.context,
                None,
            )
            .await;
        let reservations = out
            .lock()
            .map(|mut values| std::mem::take(&mut *values))
            .unwrap_or_default();
        match result {
            Ok(()) => {
                // Only after the fault is durable.
                let faulted = faulted
                    .lock()
                    .map(|mut values| std::mem::take(&mut *values))
                    .unwrap_or_default();
                for (record, message) in faulted {
                    (self.report)(message);
                    self.report_fixed(
                        crate::harness::types::ReportInfo::new(
                            crate::harness::types::ReportKind::TaskFaulted,
                        )
                        .with_task(record.id, record.kind)
                        .with_reason("restart_limit"),
                    );
                }
                Ok(reservations)
            }
            Err(error) => {
                for reservation in reservations {
                    if let Ok(mut map) = self.invocations.lock() {
                        map.remove(&reservation.invocation.task_id);
                    }
                    self.end(&reservation.invocation);
                }
                Err(error)
            }
        }
    }

    async fn reserve_in(
        self: &Arc<Self>,
        tx: &mut Transaction,
        out: &Arc<Mutex<Vec<Reservation>>>,
        faulted: &Arc<Mutex<Vec<(TaskRecord, String)>>>,
    ) -> SessionResult<()> {
        if !self.enabled.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.load_scopes(false).await?;
        let owned = self.owned_live(None);
        let records: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        let mut snapshot: Option<Arc<dyn RegistrySnapshot>> = None;
        for record in records {
            let has_invocation = self
                .invocations
                .lock()
                .map(|map| map.contains_key(&record.id))
                .unwrap_or(false);
            if has_invocation || !self.waiting_on(&record, &owned).is_empty() {
                continue;
            }
            if matches!(record.state, TaskState::Completing { .. }) {
                continue;
            }
            if let TaskState::Sleeping { until, .. } = &record.state
                && !record.abort_requested
                && *until > (self.now)()
            {
                continue;
            }
            if self.parked(&record) {
                continue;
            }
            if !record.abort_requested
                && !record.background
                && self.below_cancelled(parent_of(&record))
            {
                self.cascade_pending.store(true, Ordering::SeqCst);
                self.schedule_reconcile();
                continue;
            }
            let mode = if record.abort_requested {
                InvocationMode::Abort
            } else {
                InvocationMode::Run
            };
            if mode == InvocationMode::Abort
                && let Some(limit) = self.restart_limit().limit()
                && record.restarts.unwrap_or(0) as u64 >= limit
            {
                let message = format!(
                    "Abort handler of task {} ({}) was cut off {} times",
                    record.id,
                    record.kind,
                    record.restarts.unwrap_or(0)
                );
                self.terminate(
                    tx,
                    &record,
                    TaskOutcome::Faulted {
                        error: crate::types::TaskOutcomeError {
                            message: message.clone(),
                            detail: Some(serde_json::json!({ "reason": "restart_limit" })),
                        },
                    },
                )
                .await?;
                if let Ok(mut values) = faulted.lock() {
                    values.push((record, message));
                }
                continue;
            }
            let snap = match &snapshot {
                Some(snapshot) => Arc::clone(snapshot),
                None => {
                    let fresh = self.registry.snapshot();
                    snapshot = Some(Arc::clone(&fresh));
                    fresh
                }
            };
            let resolution = self.resolve(&record, &snap);
            match resolution {
                Resolution::Blocked { reason, .. } => {
                    // An abandoned task waits for its definition, so its abort
                    // handler can clean up; only a plain request orphans it.
                    if mode == InvocationMode::Abort && record.abort_reason.is_none() {
                        self.terminate(
                            tx,
                            &record,
                            TaskOutcome::Orphaned {
                                reason: blocked_reason_str(reason).to_string(),
                            },
                        )
                        .await?;
                    }
                    continue;
                }
                Resolution::Ready {
                    task,
                    record: resolved,
                } => {
                    if *resolved != record || resolved.status() != TaskStatus::Running {
                        let checkpoint = record_checkpoint(&resolved);
                        tx.set_task(with_state(&resolved, TaskState::Running { checkpoint }))?;
                    }
                    let invocation = self.create_invocation(&record, mode);
                    if let Ok(mut values) = out.lock() {
                        values.push(Reservation {
                            invocation,
                            task,
                            snapshot: Arc::clone(&snap),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn waiting_on(
        self: &Arc<Self>,
        record: &TaskRecord,
        owned: &BTreeMap<TaskId, Vec<TaskId>>,
    ) -> Vec<TaskId> {
        if record.abort_requested {
            return owned.get(&record.id).cloned().unwrap_or_default();
        }
        match &record.state {
            TaskState::Waiting { on, .. } => on
                .iter()
                .filter(|id| {
                    self.live
                        .lock()
                        .map(|live| live.contains_key(id))
                        .unwrap_or(false)
                })
                .copied()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn resolve(
        self: &Arc<Self>,
        record: &TaskRecord,
        snapshot: &Arc<dyn RegistrySnapshot>,
    ) -> Resolution {
        let fit = self.fit(record, snapshot.task(&record.kind));
        match fit {
            Fit::Blocked { reason, .. } => Resolution::Blocked { reason },
            Fit::Ready { task, migrates } => {
                if !migrates {
                    return Resolution::Ready {
                        task,
                        record: Box::new(record.clone()),
                    };
                }
                let checkpoint = record_checkpoint(record);
                match task.migrate(record.input.clone(), checkpoint, record.version) {
                    Some(Migration { input, checkpoint }) => {
                        let mut migrated = record.clone();
                        migrated.version = task.version();
                        migrated.input = input;
                        migrated.state = TaskState::Pending { checkpoint };
                        Resolution::Ready {
                            task,
                            record: Box::new(migrated),
                        }
                    }
                    None => {
                        let message = format!(
                            "Task {} version {} has no migration from {}",
                            record.kind,
                            task.version(),
                            record.version
                        );
                        if let Ok(mut map) = self.failed_migrations.lock() {
                            map.insert(record.id, (Arc::clone(&task), message.clone()));
                        }
                        (self.report)(message.clone());
                        self.report_fixed(
                            crate::harness::types::ReportInfo::new(
                                crate::harness::types::ReportKind::MigrationFailed,
                            )
                            .with_task(record.id, record.kind.clone()),
                        );
                        // Blocked now without a commit: the graph and the host's
                        // wake must hear it after this line releases.
                        let notifier = Arc::clone(self);
                        tokio::spawn(async move {
                            notifier.notify_blocked();
                        });
                        Resolution::Blocked {
                            reason: BlockedReason::MigrationFailed,
                        }
                    }
                }
            }
        }
    }

    fn fit(self: &Arc<Self>, record: &TaskRecord, task: Option<AnyTask>) -> Fit {
        self.fit_subject(record.id, record.version, task)
    }

    fn fit_subject(&self, id: TaskId, record_version: u64, task: Option<AnyTask>) -> Fit {
        let Some(task) = task else {
            return Fit::Blocked {
                reason: BlockedReason::MissingTask,
                error: None,
            };
        };
        let version = task.version();
        if version == record_version {
            return Fit::Ready {
                task,
                migrates: false,
            };
        }
        if version < record_version {
            return Fit::Blocked {
                reason: BlockedReason::TaskTooOld,
                error: None,
            };
        }
        let failed = self
            .failed_migrations
            .lock()
            .ok()
            .and_then(|map| map.get(&id).cloned());
        if let Some((failed_task, error)) = failed
            && Arc::ptr_eq(&failed_task, &task)
        {
            return Fit::Blocked {
                reason: BlockedReason::MigrationFailed,
                error: Some(error),
            };
        }
        Fit::Ready {
            task,
            migrates: true,
        }
    }

    fn create_invocation(
        self: &Arc<Self>,
        record: &TaskRecord,
        mode: InvocationMode,
    ) -> Arc<Invocation> {
        let controller = CancellationToken::new();
        let context = crate::chord::context::with_context_value(
            &crate::chord::context::TELEMETRY_CONTEXT_KEY,
            self.telemetry.clone(),
            &with_abort_signal(controller.clone(), &self.context),
        );
        let (done_tx, _done_rx) = tokio::sync::watch::channel(false);
        let links = if record.started_at.is_none() {
            Some(record.links.clone())
        } else {
            None
        };
        let invocation = Arc::new(Invocation {
            task_id: record.id,
            conversation_id: record.conversation_id,
            mode,
            trace: record.trace.clone(),
            links,
            terminal_outcome: Mutex::new(None),
            controller,
            context,
            watches: Mutex::new(Vec::new()),
            timers: Mutex::new(Vec::new()),
            next_timer_id: AtomicU64::new(1),
            ended: AtomicBool::new(false),
            done_tx,
            notify: Notify::new(),
        });
        if let Ok(mut map) = self.invocations.lock() {
            map.insert(record.id, Arc::clone(&invocation));
        }
        invocation
    }

    fn start(self: &Arc<Self>, reservation: &Reservation) {
        let this = Arc::clone(self);
        let reservation = reservation.clone();
        tokio::spawn(async move {
            let invocation = Arc::clone(&reservation.invocation);
            let outcome = if reservation.invocation.mode == InvocationMode::Run {
                this.run(&reservation).await
            } else {
                this.run_abort(&reservation).await
            };
            if let Err(error) = outcome {
                (this.report)(error.to_string());
            }
            this.end(&invocation);
            invocation.finish();
            this.kick();
        });
    }

    async fn run(self: &Arc<Self>, reservation: &Reservation) -> SessionResult<()> {
        let invocation = Arc::clone(&reservation.invocation);
        let task = Arc::clone(&reservation.task);
        let snapshot_cell = Arc::new(Mutex::new(Arc::clone(&reservation.snapshot)));
        let reported: Arc<Mutex<Option<Option<AnyTask>>>> = Arc::new(Mutex::new(None));
        let initial = Decider::Run {
            previous: None,
            task: Arc::clone(&task),
            snapshot: Arc::clone(&snapshot_cell),
            reported: Arc::clone(&reported),
        };
        let mut current = self.step(&invocation, initial, &invocation.context).await?;
        while let Some(ready) = current {
            // Close may seal between the decision and dispatch.
            if self.closing.load(Ordering::SeqCst) {
                return Ok(());
            }
            let phase = ready.phase().unwrap_or("").to_string();
            let checkpoint = ready.checkpoint().cloned().unwrap_or(JsonValue::Null);
            let span = self.phase_span(&invocation, &ready);
            let phase_context = crate::chord::context::with_context_value(
                &crate::chord::context::TELEMETRY_CONTEXT_KEY,
                span.context(),
                &invocation.context,
            );
            let runtime = InvocationRuntime {
                inner: Arc::clone(self),
                invocation: Arc::clone(&invocation),
                snapshot: Arc::clone(&snapshot_cell),
                agent: Arc::new(tokio::sync::OnceCell::new()),
            };
            let started = (self.now)();
            let outcome = task
                .run_phase(&phase, ready.clone(), &runtime, &phase_context)
                .await;
            let previous = PhaseResult {
                checkpoint,
                phase: phase.clone(),
                failure: outcome.as_ref().err().map(|error| error.to_string()),
            };
            let decider = Decider::Run {
                previous: Some(previous.clone()),
                task: Arc::clone(&task),
                snapshot: Arc::clone(&snapshot_cell),
                reported: Arc::clone(&reported),
            };
            let next = self.step(&invocation, decider, &phase_context).await;
            let terminal = self.phase_outcome(
                &invocation,
                next.as_ref().ok().and_then(|next| next.as_ref()),
            );
            span.context().record_metric(
                "pi.durable.phase.duration",
                ((self.now)() - started).max(0.0),
                serde_json::json!({
                    "task": task.name(),
                    "phase": phase,
                    "outcome": terminal,
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
            if let Some(failure) = self.phase_status_error(&invocation, Some(&previous)) {
                span.set_status("error", Some(&failure));
            }
            span.end(
                serde_json::json!({ "outcome": terminal })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            // A failed commit or a failed Session also ends the phase: the span
            // and its metric carry the outcome before the error returns.
            current = next?;
        }
        Ok(())
    }

    /// A phase invocation's span, with its stored parent, first-invocation
    /// links, and `resumed` marker.
    fn phase_span(&self, invocation: &Invocation, task: &RunningTask) -> TelemetrySpan {
        let parent = invocation
            .trace
            .as_ref()
            .map(|carrier| self.telemetry.extract(carrier))
            .unwrap_or_else(|| self.telemetry.clone());
        let links = invocation.links.clone().unwrap_or_default();
        let resumed = self
            .resumed
            .lock()
            .map(|mut resumed| resumed.remove(&invocation.task_id))
            .unwrap_or(false);
        let checkpoint = task.checkpoint().cloned().unwrap_or(JsonValue::Null);
        let mut attributes = serde_json::json!({
            "task.id": invocation.task_id.get(),
            "task.name": task.kind(),
            "phase": task.phase().unwrap_or(""),
            "kind": if invocation.mode == InvocationMode::Abort { "abort" } else { "run" },
            "resumed": resumed,
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        if let Some(attempt) = checkpoint.get("attempt").and_then(JsonValue::as_u64) {
            attributes.insert("attempt".to_string(), JsonValue::from(attempt));
        }
        parent.start_span("pi.durable.phase", attributes, links)
    }

    /// The end outcome of a finished phase, from its terminal outcome, the
    /// next state, or the live record it left behind.
    fn phase_outcome(&self, invocation: &Invocation, next: Option<&RunningTask>) -> String {
        if let Some(outcome) = invocation
            .terminal_outcome
            .lock()
            .ok()
            .and_then(|outcome| outcome.clone())
        {
            return match outcome {
                TaskOutcome::Completed { .. } => "done".to_string(),
                TaskOutcome::Aborted { .. } => "aborted".to_string(),
                TaskOutcome::Failed { .. }
                | TaskOutcome::Orphaned { .. }
                | TaskOutcome::Faulted { .. } => "faulted".to_string(),
            };
        }
        if let Some(next) = next {
            return format!("next={}", next.phase().unwrap_or(""));
        }
        let record = self
            .live
            .lock()
            .ok()
            .and_then(|live| live.get(&invocation.task_id).cloned());
        match record.as_ref().map(|record| record.status()) {
            Some(TaskStatus::Waiting) => "waiting".to_string(),
            Some(TaskStatus::Sleeping) => "sleeping".to_string(),
            _ if invocation.ended.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) => {
                "aborted".to_string()
            }
            // Upstream's `next=${checkpoint.phase}`: the phase the record would
            // run next, which a failed commit leaves in place.
            Some(_) => match record
                .as_ref()
                .and_then(|record| record_checkpoint(record).get("phase").cloned())
                .and_then(|phase| phase.as_str().map(str::to_string))
            {
                Some(phase) => format!("next={phase}"),
                None => "done".to_string(),
            },
            None => "done".to_string(),
        }
    }

    /// The phase-span error message for a terminal or thrown outcome.
    fn phase_status_error(
        &self,
        invocation: &Invocation,
        previous: Option<&PhaseResult>,
    ) -> Option<String> {
        if let Some(outcome) = invocation
            .terminal_outcome
            .lock()
            .ok()
            .and_then(|outcome| outcome.clone())
        {
            match outcome {
                TaskOutcome::Completed { .. } | TaskOutcome::Aborted { .. } => return None,
                TaskOutcome::Failed { error, .. } | TaskOutcome::Faulted { error } => {
                    return Some(error.message);
                }
                TaskOutcome::Orphaned { reason } => return Some(reason),
            }
        }
        if invocation.ended.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            return None;
        }
        previous.and_then(|previous| previous.failure.clone())
    }

    async fn run_abort(self: &Arc<Self>, reservation: &Reservation) -> SessionResult<()> {
        let invocation = &reservation.invocation;
        if self.closing.load(Ordering::SeqCst) {
            return Ok(());
        }
        let current = self
            .live
            .lock()
            .ok()
            .and_then(|live| live.get(&invocation.task_id).cloned())
            .filter(|record| record.status() == TaskStatus::Running)
            .map(RunningTask::new);
        let Some(current) = current else {
            return Ok(());
        };
        let span = self.phase_span(invocation, &current);
        let phase_context = crate::chord::context::with_context_value(
            &crate::chord::context::TELEMETRY_CONTEXT_KEY,
            span.context(),
            &invocation.context,
        );
        let runtime = InvocationRuntime {
            inner: Arc::clone(self),
            invocation: Arc::clone(invocation),
            snapshot: Arc::new(Mutex::new(Arc::clone(&reservation.snapshot))),
            agent: Arc::new(tokio::sync::OnceCell::new()),
        };
        let started = (self.now)();
        let failure = reservation
            .task
            .abort(current.clone(), &runtime, &phase_context)
            .await
            .err()
            .map(|error| error.to_string());
        let next = self
            .step(
                invocation,
                Decider::Abort {
                    failure: failure.clone(),
                },
                &phase_context,
            )
            .await;
        let outcome = self.phase_outcome(
            invocation,
            next.as_ref().ok().and_then(|next| next.as_ref()),
        );
        // An abort phase counts like a run phase: the histogram must not lose
        // cancels and restarts (upstream `#phase` wraps both).
        span.context().record_metric(
            "pi.durable.phase.duration",
            ((self.now)() - started).max(0.0),
            serde_json::json!({
                "task": reservation.task.name(),
                "phase": current.phase().unwrap_or(""),
                "outcome": outcome,
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        if let Some(failure) = self.phase_status_error(invocation, None) {
            span.set_status("error", Some(&failure));
        }
        span.end(
            serde_json::json!({ "outcome": outcome })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        next?;
        Ok(())
    }

    fn decide(
        self: &Arc<Self>,
        decider: &Decider,
        tx: &mut Transaction,
        current: &RunningTask,
        context: &Context,
    ) -> SessionResult<Decision> {
        match decider {
            Decider::Abort { failure } => {
                let message = failure.clone().unwrap_or_else(|| {
                    format!(
                        "Abort handler of task {} returned without a terminal outcome",
                        current.id()
                    )
                });
                Ok(Decision::Fault(message))
            }
            Decider::Run {
                previous,
                task,
                snapshot,
                reported,
            } => {
                if current.record.abort_requested {
                    return Ok(Decision::End);
                }
                let Some(previous) = previous else {
                    return Ok(Decision::Continue);
                };
                if let Some(failure) = &previous.failure {
                    return Ok(Decision::Fault(failure.clone()));
                }
                let checkpoint = current.checkpoint().cloned().unwrap_or(JsonValue::Null);
                if json_equal(&checkpoint, &previous.checkpoint) {
                    return Ok(Decision::Fault(format!(
                        "Task {} phase {} returned without durable progress",
                        current.kind(),
                        previous.phase
                    )));
                }
                let next_snapshot = self.registry.snapshot();
                if let Ok(mut slot) = snapshot.lock() {
                    *slot = Arc::clone(&next_snapshot);
                }
                let next = next_snapshot.task(current.kind());
                let same = next
                    .as_ref()
                    .map(|next| Arc::ptr_eq(next, task))
                    .unwrap_or(false);
                if !same {
                    if let Some(next) = &next
                        && can_reserve(next, &current.record)
                        && let Some(checkpoint) = current.checkpoint().cloned()
                    {
                        let next = with_state(&current.record, TaskState::Pending { checkpoint });
                        tx.set_task(next)?;
                        return Ok(Decision::End);
                    }
                    let already = {
                        let reported = reported.lock().map_err(|_| {
                            SessionError::message("scheduler report state poisoned")
                        })?;
                        match &*reported {
                            None => false,
                            Some(Some(previous)) => next
                                .as_ref()
                                .is_some_and(|next| Arc::ptr_eq(previous, next)),
                            Some(None) => next.is_none(),
                        }
                    };
                    if !already {
                        if let Ok(mut reported) = reported.lock() {
                            *reported = Some(next.clone());
                        }
                        let cause = if next.is_none() {
                            "missing_task"
                        } else {
                            "incompatible_task"
                        };
                        (self.report)(format!(
                            "Task {} keeps running under its old {} definition ({cause})",
                            current.id(),
                            current.kind()
                        ));
                        self.report_fixed_with(
                            crate::harness::types::ReportInfo::new(
                                crate::harness::types::ReportKind::TaskOutdated,
                            )
                            .with_task(current.id(), current.kind().to_string())
                            .with_reason(cause),
                            Some(context),
                        );
                    }
                }
                Ok(Decision::Continue)
            }
        }
    }

    async fn step(
        self: &Arc<Self>,
        invocation: &Arc<Invocation>,
        decider: Decider,
        context: &Context,
    ) -> SessionResult<Option<RunningTask>> {
        let this = Arc::clone(self);
        let inv = Arc::clone(invocation);
        let telemetry_context = context.clone();
        let result: SessionResult<Option<RunningTask>> = self
            .session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    let inv = Arc::clone(&inv);
                    let decider = decider.clone();
                    let telemetry_context = telemetry_context.clone();
                    Box::pin(async move {
                        let found = this
                            .live
                            .lock()
                            .ok()
                            .and_then(|live| live.get(&inv.task_id).cloned());
                        let current = found
                            .filter(|record| record.status() == TaskStatus::Running)
                            .map(RunningTask::new);
                        let decision = match &current {
                            Some(current) if !this.closing.load(Ordering::SeqCst) => {
                                this.decide(&decider, tx, current, &telemetry_context)?
                            }
                            _ => Decision::End,
                        };
                        if matches!(decision, Decision::Continue) {
                            return Ok(current);
                        }
                        this.end(&inv);
                        if let Decision::Fault(message) = decision
                            && let Some(current) = &current
                        {
                            this.report_fixed_with(
                                crate::harness::types::ReportInfo::new(
                                    crate::harness::types::ReportKind::TaskFaulted,
                                )
                                .with_task(current.id(), current.kind().to_string())
                                .with_reason("threw"),
                                Some(&telemetry_context),
                            );
                            let outcome = TaskOutcome::Faulted {
                                error: TaskOutcomeError {
                                    message,
                                    detail: None,
                                },
                            };
                            if let Ok(mut slot) = inv.terminal_outcome.lock() {
                                *slot = Some(outcome.clone());
                            }
                            this.terminate(tx, &current.record, outcome).await?;
                        }
                        Ok(None)
                    })
                }),
                &crate::chord::context::without_abort_signal(context),
                None,
            )
            .await;
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.end(invocation);
                self.fail_session(error);
                Ok(None)
            }
        }
    }

    async fn terminate(
        self: &Arc<Self>,
        tx: &mut Transaction,
        record: &TaskRecord,
        outcome: TaskOutcome,
    ) -> SessionResult<()> {
        self.load_scopes(false).await?;
        let overlay = overlay_of(tx);
        if self.owned_live(Some(&overlay)).contains_key(&record.id) {
            tx.set_task(with_state(
                record,
                TaskState::Completing {
                    outcome: outcome.clone(),
                },
            ))?;
            return Ok(());
        }
        tx.set_task(with_state(
            record,
            TaskState::Terminal {
                outcome: outcome.clone(),
            },
        ))?;
        if let Some(scheduler_outcome) = scheduler_outcome(&outcome) {
            settle_scheduler_outcome(tx, record, &scheduler_outcome).await?;
        }
        Ok(())
    }

    async fn commit_state(
        self: &Arc<Self>,
        tx: &mut Transaction,
        invocation: &Arc<Invocation>,
        current: &RunningTask,
        next: NextTaskState,
    ) -> SessionResult<()> {
        if let NextTaskState::Terminal { outcome } = &next
            && let Ok(mut slot) = invocation.terminal_outcome.lock()
        {
            *slot = Some(outcome.clone());
        }
        if let NextTaskState::Waiting { on, policy, .. } = &next {
            self.validate_wait(tx, invocation, current, on, *policy)
                .await?;
        }
        if let NextTaskState::Sleeping { until, .. } = &next {
            // Upstream `validateSleep`: a time on the Harness clock, and never
            // in an abort handler (whose cut-off budget could otherwise reset
            // on every sleep).
            if invocation.mode == InvocationMode::Abort {
                return Err(SessionError::message(format!(
                    "Abort handler of task {} cannot sleep",
                    current.id()
                )));
            }
            if !until.is_finite() || *until < 0.0 {
                return Err(SessionError::message(format!(
                    "Task {} cannot sleep until {until}: not a time in milliseconds",
                    current.id()
                )));
            }
        }
        if let NextTaskState::Terminal { outcome } = &next {
            let overlay = overlay_of(tx);
            self.load_scopes(false).await?;
            let candidates: Vec<TaskRecord> = overlay.tasks.values().cloned().collect();
            for record in candidates {
                self.load_chain(parent_of(&record), Some(&overlay)).await?;
            }
            if self.owned_live(Some(&overlay)).contains_key(&current.id()) {
                tx.set_task(with_state(
                    &progressed(&current.record),
                    TaskState::Completing {
                        outcome: outcome.clone(),
                    },
                ))?;
                return Ok(());
            }
        }
        let state = match next {
            NextTaskState::Running { checkpoint } => TaskState::Running { checkpoint },
            NextTaskState::Waiting {
                checkpoint,
                on,
                policy,
            } => TaskState::Waiting {
                checkpoint,
                on,
                policy,
            },
            NextTaskState::Sleeping { checkpoint, until } => {
                TaskState::Sleeping { checkpoint, until }
            }
            NextTaskState::Terminal { outcome } => TaskState::Terminal { outcome },
        };
        // A committed state that differs from the current one is progress: the
        // restart count starts over. Committing the same checkpoint again, as
        // intent before an effect, is not.
        let same =
            serde_json::to_value(&current.record.state).ok() == serde_json::to_value(&state).ok();
        let base = if same {
            current.record.clone()
        } else {
            progressed(&current.record)
        };
        tx.set_task(with_state(&base, state))?;
        Ok(())
    }

    async fn validate_wait(
        self: &Arc<Self>,
        tx: &Transaction,
        invocation: &Arc<Invocation>,
        current: &RunningTask,
        on: &[TaskId],
        policy: JoinPolicy,
    ) -> SessionResult<()> {
        if invocation.mode == InvocationMode::Abort {
            return Err(SessionError::message(format!(
                "Abort handler of task {} cannot wait",
                current.id()
            )));
        }
        let overlay = overlay_of(tx);
        self.load_chain(parent_of(&current.record), Some(&overlay))
            .await?;
        let mut owners: BTreeSet<TaskId> = BTreeSet::new();
        for step in self.above(parent_of(&current.record), Some(&overlay)) {
            if let Step::Task(id, _) = step {
                owners.insert(id);
            }
        }
        for id in on {
            if *id == current.id() || owners.contains(id) {
                return Err(SessionError::message(format!(
                    "Task {} cannot wait on itself or its owner {}",
                    current.id(),
                    id
                )));
            }
            let member = overlay
                .tasks
                .get(id)
                .cloned()
                .or_else(|| self.live.lock().ok().and_then(|live| live.get(id).cloned()));
            let member = match member {
                Some(member) => Some(member),
                None => self.storage.task(*id, &self.context).await?,
            };
            let Some(member) = member else {
                return Err(SessionError::message(format!("Task {id} does not exist")));
            };
            if policy == JoinPolicy::FailFast && member.owner != Some(current.id()) {
                return Err(SessionError::message(format!(
                    "Task {} can wait failFast only on tasks it owns; {} is not one",
                    current.id(),
                    id
                )));
            }
        }
        Ok(())
    }

    fn end(self: &Arc<Self>, invocation: &Arc<Invocation>) {
        if invocation.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut map) = self.invocations.lock() {
            let remove = map
                .get(&invocation.task_id)
                .map(|current| Arc::ptr_eq(current, invocation))
                .unwrap_or(false);
            if remove {
                map.remove(&invocation.task_id);
            }
        }
        if let Ok(mut watches) = invocation.watches.lock() {
            for watch in watches.drain(..) {
                watch.cancel();
            }
        }
        let timers = invocation
            .timers
            .lock()
            .map(|mut timers| std::mem::take(&mut *timers))
            .unwrap_or_default();
        for (_, disarm) in timers {
            disarm();
        }
        invocation.controller.cancel();
    }

    // ─── Inspection ─────────────────────────────────────────────────────────

    async fn inspect(
        self: &Arc<Self>,
        snapshot: Arc<dyn RegistrySnapshot>,
    ) -> SessionResult<(Scheduling, Vec<TaskInspection>)> {
        self.load_scopes(false).await?;
        let owned = self.owned_live(None);
        let records: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        let mut tasks = Vec::new();
        for record in records {
            let state = self.inspect_task(&record, &snapshot, &owned);
            tasks.push(TaskInspection { record, state });
        }
        let scheduling = if self.closing.load(Ordering::SeqCst) {
            Scheduling::Closing
        } else if self.enabled.load(Ordering::SeqCst) {
            Scheduling::Running
        } else {
            Scheduling::Paused
        };
        Ok((scheduling, tasks))
    }

    fn inspect_task(
        self: &Arc<Self>,
        record: &TaskRecord,
        snapshot: &Arc<dyn RegistrySnapshot>,
        owned: &BTreeMap<TaskId, Vec<TaskId>>,
    ) -> TaskInspectionState {
        let running = self
            .invocations
            .lock()
            .map(|map| map.contains_key(&record.id))
            .unwrap_or(false);
        if running {
            return TaskInspectionState::Running;
        }
        if matches!(record.state, TaskState::Completing { .. }) {
            return TaskInspectionState::Completing;
        }
        let on = self.waiting_on(record, owned);
        if !on.is_empty() {
            return TaskInspectionState::Waiting { on };
        }
        if let Some(until) = not_due(record, (self.now)()) {
            return TaskInspectionState::Sleeping { until };
        }
        if self.parked(record) {
            return TaskInspectionState::Blocked {
                reason: BlockedReason::RestartLimit,
                error: None,
            };
        }
        match self.fit(record, snapshot.task(&record.kind)) {
            Fit::Blocked { reason, error } => TaskInspectionState::Blocked { reason, error },
            Fit::Ready { task, migrates } => {
                if migrates && !task.has_migration() {
                    TaskInspectionState::Blocked {
                        reason: BlockedReason::MigrationFailed,
                        error: Some(format!(
                            "Task {} version {} has no migration from {}",
                            record.kind,
                            task.version(),
                            record.version
                        )),
                    }
                } else {
                    TaskInspectionState::Ready { migrates }
                }
            }
        }
    }

    // ─── Task creation ──────────────────────────────────────────────────────

    async fn start_run(
        self: &Arc<Self>,
        tx: &mut Transaction,
        conversation_id: ConversationId,
        live: &DocDraft,
        inputs: Vec<SubmissionId>,
    ) -> SessionResult<()> {
        let snapshot = self.registry.snapshot();
        let (version, checkpoint) = match snapshot.task(GENERATION_TASK_NAME) {
            Some(definition) => (definition.version(), definition.initial()),
            None => (1, serde_json::json!({ "phase": "prepare", "attempt": 1 })),
        };
        let (trace, links) = tx.task_trace(&inputs).await?;
        let task_id = tx
            .create_task(TaskCreate {
                kind: GENERATION_TASK_NAME.to_string(),
                version,
                input: JsonValue::Object(JsonObject::new()),
                checkpoint,
                ownership: TaskOwnership::Conversation(conversation_id),
                conversation_id: Some(conversation_id),
                background: false,
                abandon_on_restart: false,
                trace,
                links,
            })
            .await?;
        let mut state: LiveState = read_state(live)?;
        state.run = Some(LiveRun { task_id, inputs });
        write_state(live, &state)
    }

    async fn compact(
        self: &Arc<Self>,
        tx: &mut Transaction,
        conversation_id: ConversationId,
        instructions: Option<String>,
    ) -> SessionResult<TaskId> {
        let snapshot = self.registry.snapshot();
        let (version, checkpoint) = match snapshot.task(COMPACTION_TASK_NAME) {
            Some(definition) => (definition.version(), definition.initial()),
            None => (1, serde_json::json!({ "phase": "prepare" })),
        };
        let input = match instructions {
            Some(instructions) => {
                serde_json::json!({ "reason": "manual", "instructions": instructions })
            }
            None => serde_json::json!({ "reason": "manual" }),
        };
        tx.create_task(TaskCreate {
            kind: COMPACTION_TASK_NAME.to_string(),
            version,
            input,
            checkpoint,
            ownership: TaskOwnership::Conversation(conversation_id),
            conversation_id: Some(conversation_id),
            background: false,
            abandon_on_restart: false,
            trace: Default::default(),
            links: Vec::new(),
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Runtime façade
// ---------------------------------------------------------------------------

/// Document reader handed to an environment factory.
struct SchedulerDocumentReader {
    session: Arc<SessionImpl>,
}

#[async_trait]
impl DocumentReader for SchedulerDocumentReader {
    async fn snapshot(
        &self,
        token: &DocToken,
        owner: DocOwner<'_>,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        self.session.snapshot(token, owner, context).await
    }
}

/// Operations of one task invocation (upstream `#runtime`).
#[derive(Clone)]
struct InvocationRuntime {
    inner: Arc<SchedulerInner>,
    invocation: Arc<Invocation>,
    snapshot: Arc<Mutex<Arc<dyn RegistrySnapshot>>>,
    /// Shared so a clone (the owned handle for spawned progress commits)
    /// resolves the agent once per invocation.
    agent: Arc<tokio::sync::OnceCell<Result<Agent, String>>>,
}

/// The boxed change `gated` commits with.
type GatedFn<T> = dyn for<'a> Fn(
        &'a mut Transaction,
        RunningTask,
    ) -> crate::harness::types::BoxFuture<'a, SessionResult<T>>
    + Send
    + Sync;

impl InvocationRuntime {
    fn ensure_open(&self) -> SessionResult<()> {
        if self.invocation.ended.load(Ordering::SeqCst) {
            Err(ended_error(self.invocation.task_id))
        } else {
            Ok(())
        }
    }

    fn current_snapshot(&self) -> Arc<dyn RegistrySnapshot> {
        self.snapshot
            .lock()
            .map(|slot| Arc::clone(&slot))
            .unwrap_or_else(|_| self.inner.registry.snapshot())
    }

    /// Commit after rereading the task on the line and gating the invocation.
    async fn gated<T: Send + 'static>(
        &self,
        change: Arc<GatedFn<T>>,
        context: &Context,
    ) -> SessionResult<T> {
        self.ensure_open()?;
        let inner = Arc::clone(&self.inner);
        let invocation = Arc::clone(&self.invocation);
        let conversation_id = invocation.conversation_id;
        let task_id = invocation.task_id;
        let session = Arc::clone(&inner.session);
        session
            .commit_with(
                commit_fn(move |tx| {
                    let inner = Arc::clone(&inner);
                    let invocation = Arc::clone(&invocation);
                    let change = Arc::clone(&change);
                    Box::pin(async move {
                        if invocation.ended.load(Ordering::SeqCst) {
                            return Err(ended_error(invocation.task_id));
                        }
                        if inner.closing.load(Ordering::SeqCst) {
                            return Err(SessionError::Closed);
                        }
                        let found = inner
                            .live
                            .lock()
                            .ok()
                            .and_then(|live| live.get(&invocation.task_id).cloned());
                        let Some(found) = found else {
                            return Err(SessionError::message(format!(
                                "Task {} is terminal",
                                invocation.task_id
                            )));
                        };
                        if found.status() != TaskStatus::Running {
                            return Err(SessionError::message(format!(
                                "Task {} is {}",
                                invocation.task_id,
                                found.status().as_str()
                            )));
                        }
                        if invocation.mode == InvocationMode::Run && found.abort_requested {
                            return Err(SessionError::message(format!(
                                "Task {} has a durable abort mark",
                                invocation.task_id
                            )));
                        }
                        let current = RunningTask::new(found);
                        change(tx, current).await
                    })
                }),
                context,
                Some(TransactionScope {
                    conversation_id: Some(conversation_id),
                    task_id: Some(task_id),
                }),
            )
            .await
    }
}

#[async_trait]
impl TaskRuntime for InvocationRuntime {
    fn task_id(&self) -> TaskId {
        self.invocation.task_id
    }

    fn conversation_id(&self) -> ConversationId {
        self.invocation.conversation_id
    }

    fn signal(&self) -> CancellationToken {
        self.invocation.controller.clone()
    }

    fn registry(&self) -> Arc<dyn RegistrySnapshot> {
        self.current_snapshot()
    }

    fn models(&self) -> Models {
        self.inner.models.current()
    }

    fn settings(&self) -> Settings {
        self.inner.settings()
    }

    async fn agent(&self, context: &Context) -> SessionResult<Agent> {
        self.ensure_open()?;
        let conversation_id = self.invocation.conversation_id;
        let invocation_context = self.invocation.context.clone();
        let snapshot = self.current_snapshot();
        let inner = Arc::clone(&self.inner);
        let cell = Arc::clone(&self.agent);
        let future = cell.get_or_init(|| async move {
            inner
                .resolve_agent(conversation_id, snapshot, &invocation_context)
                .await
                .map_err(|error| error.to_string())
        });
        match await_with_context(future, context).await {
            Ok(Ok(agent)) => Ok(agent.clone()),
            Ok(Err(message)) => Err(SessionError::message(message)),
            Err(_) => Err(SessionError::Aborted),
        }
    }

    async fn env(&self, context: &Context) -> SessionResult<Option<Arc<dyn ExecutionEnv>>> {
        self.ensure_open()?;
        self.inner
            .build_env(self.invocation.conversation_id, context)
            .await
    }

    async fn commit(&self, change: TaskCommitFn, context: &Context) -> SessionResult<()> {
        let inner = Arc::clone(&self.inner);
        let invocation = Arc::clone(&self.invocation);
        let telemetry = context.telemetry();
        let changed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let changed_in_commit = Arc::clone(&changed);
        let wrapper: Arc<GatedFn<()>> = Arc::new(move |tx, current| {
            let change = Arc::clone(&change);
            let inner = Arc::clone(&inner);
            let invocation = Arc::clone(&invocation);
            let changed = Arc::clone(&changed_in_commit);
            Box::pin(async move {
                let next = change(tx, current.clone()).await?;
                let checkpoint = next.as_ref().and_then(next_running_phase);
                let previous_phase = current.phase().map(str::to_string);
                if let Some(next) = next {
                    inner.commit_state(tx, &invocation, &current, next).await?;
                }
                if let Some(checkpoint) = checkpoint
                    && previous_phase.as_deref() != Some(checkpoint.as_str())
                    && let Ok(mut slot) = changed.lock()
                {
                    *slot = Some(checkpoint);
                }
                Ok(())
            })
        });
        let result = self.gated(wrapper, context).await;
        if result.is_ok()
            && let Some(checkpoint) = changed.lock().ok().and_then(|mut slot| slot.take())
        {
            telemetry.add_event(
                "pi.durable.checkpoint",
                serde_json::json!({ "phase": checkpoint })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        result
    }

    async fn memo(&self, name: &str, context: &Context) -> SessionResult<Option<JsonValue>> {
        let _ = context;
        self.ensure_open()?;
        let record = self
            .inner
            .live
            .lock()
            .ok()
            .and_then(|live| live.get(&self.invocation.task_id).cloned());
        Ok(memo_of(record.as_ref(), name))
    }

    async fn memo_set(
        &self,
        name: &str,
        value: JsonValue,
        context: &Context,
    ) -> SessionResult<JsonValue> {
        let name = name.to_string();
        let candidate = value.clone();
        let wrapper: Arc<GatedFn<JsonValue>> = Arc::new(move |tx, current| {
            let name = name.clone();
            let candidate = candidate.clone();
            Box::pin(async move {
                if let Some(winner) = memo_of(Some(&current.record), &name) {
                    return Ok(winner);
                }
                let mut record = current.record.clone();
                let mut memos = record.memos.take().unwrap_or_default();
                memos.insert(name, candidate.clone());
                record.memos = Some(memos);
                tx.set_task(record)?;
                Ok(candidate)
            })
        });
        self.gated(wrapper, context).await
    }

    async fn create_task(
        &self,
        definition: AnyTask,
        input: JsonValue,
        ownership: TaskOwnership,
        background: bool,
        context: &Context,
    ) -> SessionResult<TaskId> {
        let conversation_id = self.invocation.conversation_id;
        let wrapper: Arc<GatedFn<TaskId>> = Arc::new(move |tx, _current| {
            let definition = Arc::clone(&definition);
            let input = input.clone();
            Box::pin(async move {
                tx.create_task(TaskCreate {
                    kind: definition.name().to_string(),
                    version: definition.version(),
                    input,
                    checkpoint: definition.initial(),
                    ownership,
                    conversation_id: Some(conversation_id),
                    background,
                    abandon_on_restart: false,
                    trace: Default::default(),
                    links: Vec::new(),
                })
                .await
            })
        });
        self.gated(wrapper, context).await
    }

    fn document_reader(&self) -> Arc<dyn DocumentReader> {
        Arc::new(SchedulerDocumentReader {
            session: Arc::clone(&self.inner.session),
        })
    }

    async fn provider_session_id(&self, context: &Context) -> SessionResult<String> {
        self.ensure_open()?;
        crate::harness::provider::ensure_provider_session_id(
            &self.inner.session,
            self.invocation.conversation_id,
            context,
        )
        .await
    }

    async fn get_task(&self, id: TaskId, context: &Context) -> SessionResult<Option<TaskRecord>> {
        self.ensure_open()?;
        let storage = Arc::clone(&self.inner.storage);
        let read_context = context.clone();
        self.inner
            .session
            .read_on_line(move || {
                Box::pin(async move { Ok(storage.task(id, &read_context).await?) })
            })
            .await
    }

    async fn wait_for_task(
        &self,
        id: TaskId,
        context: &Context,
    ) -> SessionResult<SettledTask<JsonValue>> {
        self.ensure_open()?;
        let bound = with_abort_signal(self.invocation.controller.clone(), context);
        self.inner.wait_for_task(id, &bound).await
    }

    async fn owned_tasks(&self, context: &Context) -> SessionResult<Vec<TaskRecord>> {
        self.ensure_open()?;
        // A cancelled caller does not get a list (upstream `throwIfAborted`).
        if let Some(signal) = context.abort_signal()
            && signal.is_cancelled()
        {
            return Err(SessionError::Aborted);
        }
        let task_id = self.invocation.task_id;
        let mut owned: Vec<TaskRecord> = self
            .inner
            .live
            .lock()
            .map(|live| {
                live.values()
                    .filter(|record| {
                        record.owner == Some(task_id)
                            && !matches!(record.state, TaskState::Terminal { .. })
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        owned.sort_by_key(|record| record.id);
        Ok(owned)
    }

    async fn abort_owned(&self, id: TaskId, context: &Context) -> SessionResult<()> {
        self.ensure_open()?;
        let bound = with_abort_signal(self.invocation.controller.clone(), context);
        let storage = Arc::clone(&self.inner.storage);
        let read_context = bound.clone();
        let record = self
            .inner
            .session
            .read_on_line(move || {
                Box::pin(async move { Ok(storage.task(id, &read_context).await?) })
            })
            .await?;
        let Some(record) = record else {
            return Err(SessionError::message(format!(
                "Task {id} is not owned by task {}",
                self.invocation.task_id
            )));
        };
        if record.owner != Some(self.invocation.task_id) {
            return Err(SessionError::message(format!(
                "Task {id} is not owned by task {}",
                self.invocation.task_id
            )));
        }
        // It may have ended between `ownedTasks()` and this call; then there
        // is nothing to abort, and waiting for it would be wasted.
        if matches!(record.state, TaskState::Terminal { .. }) {
            return Ok(());
        }
        let _ = self.inner.abort(id, &bound, true).await?;
        let _ = self.inner.wait_for_task(id, &bound).await?;
        Ok(())
    }

    async fn outcomes(
        &self,
        ids: Vec<TaskId>,
        context: &Context,
    ) -> SessionResult<Vec<TaskOutcome>> {
        self.ensure_open()?;
        let storage = Arc::clone(&self.inner.storage);
        let read_context = context.clone();
        self.inner
            .session
            .read_on_line(move || {
                Box::pin(async move {
                    let mut outcomes = Vec::new();
                    for id in ids {
                        let state = storage
                            .task(id, &read_context)
                            .await?
                            .map(|record| record.state);
                        match state {
                            Some(TaskState::Terminal { outcome }) => outcomes.push(outcome),
                            _ => {
                                return Err(SessionError::message(format!(
                                    "Task {id} is not terminal"
                                )));
                            }
                        }
                    }
                    Ok(outcomes)
                })
            })
            .await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> SessionResult<Option<Arc<dyn ConversationHandle>>> {
        self.ensure_open()?;
        let Some(submissions) = self.inner.submissions.get().cloned() else {
            return Ok(None);
        };
        let session = Arc::clone(&self.inner.session);
        let job_session = Arc::clone(&session);
        let read_context = context.clone();
        let record = session
            .read_on_line(move || {
                Box::pin(async move {
                    Ok(job_session
                        .storage()
                        .conversation(id, &read_context)
                        .await?)
                })
            })
            .await?;
        if record.is_none() {
            return Ok(None);
        }
        let tasks: Arc<dyn SchedulerService> = Arc::new(TaskScheduler {
            inner: Arc::clone(&self.inner),
        });
        let invocation = Arc::clone(&self.invocation);
        let task_id = invocation.task_id;
        let binding = InvocationBinding {
            signal: invocation.controller.clone(),
            check: Arc::new(move || {
                if invocation.ended.load(Ordering::SeqCst) {
                    Err(ended_error(task_id))
                } else {
                    Ok(())
                }
            }),
        };
        Ok(Some(bound_conversation(id, binding, submissions, tasks)))
    }

    async fn entry(
        &self,
        id: EntryId,
        context: &Context,
    ) -> SessionResult<Option<crate::types::EntryRecord>> {
        self.ensure_open()?;
        let storage = Arc::clone(&self.inner.storage);
        let conversation_id = self.invocation.conversation_id;
        let read_context = context.clone();
        self.inner
            .session
            .read_on_line(move || {
                Box::pin(async move {
                    Ok(storage
                        .entry_in_conversation(conversation_id, id, &read_context)
                        .await?
                        .map(|stored| stored.entry))
                })
            })
            .await
    }

    async fn context(
        &self,
        conversation_id: ConversationId,
        context: &Context,
        at: Option<EntryId>,
    ) -> SessionResult<crate::harness::types::ContextView> {
        self.ensure_open()?;
        let inner = Arc::clone(&self.inner);
        let invocation = Arc::clone(&self.invocation);
        let previous = inner
            .contexts
            .lock()
            .ok()
            .and_then(|contexts| contexts.get(&conversation_id).cloned())
            .map(|kept| kept.range);
        let (view, range) = read_context_from(
            &inner.session,
            &inner.storage,
            conversation_id,
            context,
            at,
            previous,
        )
        .await?;
        if let Some(range) = range
            && !inner.closing.load(Ordering::SeqCst)
            && !invocation.ended.load(Ordering::SeqCst)
        {
            let should_keep = inner
                .contexts
                .lock()
                .ok()
                .map(|contexts| {
                    contexts
                        .get(&conversation_id)
                        .map(|kept| kept.range.bounds.tail <= range.bounds.tail)
                        .unwrap_or(true)
                })
                .unwrap_or(false);
            if should_keep {
                let idle = inner.idle(Some(conversation_id));
                if !idle {
                    if let Ok(mut contexts) = inner.contexts.lock() {
                        contexts.insert(
                            conversation_id,
                            KeptContext {
                                range,
                                idle_since: None,
                            },
                        );
                    }
                } else if inner.settings().context_retention_ms > 0 {
                    let now = (inner.now)();
                    if let Ok(mut contexts) = inner.contexts.lock() {
                        contexts.insert(
                            conversation_id,
                            KeptContext {
                                range,
                                idle_since: Some(now),
                            },
                        );
                    }
                }
            }
        }
        Ok(view)
    }

    async fn snapshot(
        &self,
        token: &DocToken,
        owner: DocOwner<'_>,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        self.ensure_open()?;
        self.inner.session.snapshot(token, owner, context).await
    }

    async fn watch_doc(
        &self,
        token: &DocToken,
        owner: DocOwner<'_>,
        context: &Context,
    ) -> SessionResult<Option<CommittedWatch>> {
        self.ensure_open()?;
        let watch = self.inner.session.watch_doc(token, owner, context).await?;
        match watch {
            None => Ok(None),
            Some(watch) => {
                if self.invocation.ended.load(Ordering::SeqCst) {
                    watch.cancel();
                    return Err(ended_error(self.invocation.task_id));
                }
                if let Ok(mut watches) = self.invocation.watches.lock() {
                    watches.push(watch.clone());
                }
                Ok(Some(watch))
            }
        }
    }

    fn handle(&self) -> Arc<dyn TaskRuntime> {
        Arc::new(self.clone())
    }

    fn now(&self) -> SessionResult<f64> {
        self.ensure_open()?;
        Ok((self.inner.now)())
    }

    fn report(&self, message: String) -> SessionResult<()> {
        self.ensure_open()?;
        (self.inner.report)(message);
        Ok(())
    }

    fn report_info(&self, info: crate::harness::types::ReportInfo) {
        // Task-level reports land on the invocation's span context
        // (upstream passes `invocation.context`).
        self.inner
            .report_fixed_with(info, Some(&self.invocation.context));
    }

    fn set_timer(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        delay_ms: u64,
    ) -> SessionResult<TimerDisarm> {
        // An ended invocation's timer never runs (upstream returns a no-op
        // disarm instead of failing the call).
        if self.invocation.ended.load(Ordering::SeqCst) {
            return Ok(Arc::new(|| {}));
        }
        let ended = Arc::clone(&self.invocation);
        let callback = Arc::new(move || {
            // A host timer that ignores its disarm may still fire after the
            // phase ended; that must run nothing.
            if !ended.ended.load(Ordering::SeqCst) {
                callback();
            }
        });
        let native = self.inner.timers.foreground(callback, delay_ms as f64);
        let id = self.invocation.next_timer_id.fetch_add(1, Ordering::SeqCst);
        let invocation = Arc::clone(&self.invocation);
        let disarm: TimerDisarm = Arc::new(move || {
            native();
            if let Ok(mut timers) = invocation.timers.lock() {
                timers.retain(|(timer_id, _)| *timer_id != id);
            }
        });
        if let Ok(mut timers) = self.invocation.timers.lock() {
            timers.push((id, Arc::clone(&disarm)));
        }
        if self.invocation.ended.load(Ordering::SeqCst) {
            disarm();
        }
        Ok(disarm)
    }

    async fn sleep_until(
        &self,
        until: f64,
        checkpoint: JsonValue,
        context: &Context,
    ) -> SessionResult<()> {
        let change: TaskCommitFn = Arc::new(move |_tx, _current| {
            let checkpoint = checkpoint.clone();
            Box::pin(async move { Ok(Some(NextTaskState::Sleeping { checkpoint, until })) })
        });
        self.commit(change, context).await
    }
}

// ---------------------------------------------------------------------------
// Concrete scheduler
// ---------------------------------------------------------------------------

/// Durable task scheduler of one Harness (upstream `TaskScheduler`).
pub struct TaskScheduler {
    inner: Arc<SchedulerInner>,
}

impl TaskScheduler {
    /// Build a scheduler from its Harness internals.
    pub fn new(deps: SchedulerDeps) -> Arc<Self> {
        let scheduler = Arc::new(Self {
            inner: Arc::new(SchedulerInner::new(deps)),
        });
        // A weak capture: the Session keeps the hook, and the inner keeps the
        // Session, so a strong hook would keep both alive forever.
        let weak = Arc::downgrade(&scheduler.inner);
        let session = Arc::clone(&scheduler.inner.session);
        session.on_failure(Arc::new(move |cause| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let cause = cause.to_string();
            inner
                .task_waiters
                .reject_all(crate::harness::util::WaitError::Failed(cause.clone()));
            inner
                .idle_waiters
                .reject_all(crate::harness::util::WaitError::Failed(cause));
        }));
        scheduler
    }
}

#[async_trait]
impl SchedulerService for TaskScheduler {
    fn resume(&self) {
        self.inner.resume();
    }

    fn wake(&self) -> Wake {
        self.inner.wake_snapshot()
    }

    async fn retry_task(&self, id: TaskId, context: &Context) -> SessionResult<()> {
        self.inner.retry_task(id, context).await
    }

    async fn open(&self, context: &Context) -> SessionResult<()> {
        self.inner.open(context).await
    }

    async fn join(&self) -> SessionResult<()> {
        self.inner.join().await
    }

    async fn start_run(
        &self,
        tx: &mut Transaction,
        conversation_id: ConversationId,
        live: &DocDraft,
        inputs: Vec<SubmissionId>,
    ) -> SessionResult<()> {
        self.inner
            .start_run(tx, conversation_id, live, inputs)
            .await
    }

    async fn compact(
        &self,
        tx: &mut Transaction,
        conversation_id: ConversationId,
        instructions: Option<String>,
    ) -> SessionResult<TaskId> {
        self.inner.compact(tx, conversation_id, instructions).await
    }

    async fn abort(&self, id: TaskId, context: &Context) -> SessionResult<TaskAbort> {
        self.inner.abort(id, context, false).await
    }

    async fn abort_conversation(
        &self,
        conversation_id: ConversationId,
        background: bool,
        context: &Context,
    ) -> SessionResult<()> {
        self.inner
            .abort_conversation(conversation_id, background, context)
            .await
    }

    async fn wait_for_task(
        &self,
        id: TaskId,
        context: &Context,
    ) -> SessionResult<SettledTask<JsonValue>> {
        self.inner.wait_for_task(id, context).await
    }

    async fn wait_for_idle(
        &self,
        conversation_id: Option<ConversationId>,
        context: &Context,
    ) -> SessionResult<()> {
        self.inner.wait_for_idle(conversation_id, context).await
    }

    async fn inspect(
        &self,
        snapshot: Arc<dyn RegistrySnapshot>,
    ) -> SessionResult<(Scheduling, Vec<TaskInspection>)> {
        self.inner.inspect(snapshot).await
    }

    fn blocked_reason(&self, subject: &BlockedSubject) -> Option<BlockedReason> {
        self.inner.blocked_reason(subject)
    }

    fn subscribe_blocked(&self, listener: BlockedListener) -> Subscription {
        let id = self
            .inner
            .next_blocked_listener
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut listeners) = self.inner.blocked_listeners.lock() {
            listeners.insert(id, listener);
        }
        let inner = Arc::clone(&self.inner);
        Subscription::new(move || {
            if let Ok(mut listeners) = inner.blocked_listeners.lock() {
                listeners.remove(&id);
            }
        })
    }

    fn install_submissions(&self, submissions: Arc<Submissions>) {
        let _ = self.inner.submissions.set(submissions);
    }
}

/// Default [`SchedulerService`] used when a caller injects no scheduler. Every
/// scheduler-backed operation rejects with [`UNAVAILABLE_MESSAGE`];
/// `resume`/`join`/`open` are inert so the wait-free harness lifecycle works.
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

    fn wake(&self) -> Wake {
        Wake::default()
    }

    async fn retry_task(&self, _id: TaskId, _context: &Context) -> SessionResult<()> {
        unavailable()
    }

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

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The phase a `NextTaskState::Running` checkpoint moves to, if any.
fn next_running_phase(next: &NextTaskState) -> Option<String> {
    match next {
        NextTaskState::Running { checkpoint } => checkpoint
            .get("phase")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// A live owner's durable cancellation intent (upstream `cancellationIntent`).
fn cancellation_intent(record: &TaskRecord) -> bool {
    record.status() != TaskStatus::Terminal && (record.abort_requested || failed_outcome(record))
}

/// Stage one cascade mark (upstream `#reconcile` `mark()`): a request staged in
/// this pass wins over a `restart` mark, an already requested task is not
/// marked again, and a `restart` mark is upgraded by a request.
fn stage_mark(
    marks: &mut BTreeMap<TaskId, (TaskRecord, Option<String>)>,
    record: &TaskRecord,
    reason: Option<&str>,
) {
    if let Some((_, staged)) = marks.get(&record.id)
        && staged.is_none()
    {
        return;
    }
    if record.abort_requested && (record.abort_reason.is_none() || reason.is_some()) {
        return;
    }
    marks.insert(record.id, (record.clone(), reason.map(str::to_string)));
}

/// A sleep whose `until` the clock has not reached; an abort mark ends a sleep
/// (upstream `#notDue`).
fn not_due(record: &TaskRecord, now: f64) -> Option<f64> {
    match &record.state {
        TaskState::Sleeping { until, .. } if !record.abort_requested && *until > now => {
            Some(*until)
        }
        _ => None,
    }
}

/// Whether the record holds or ends with an outcome other than `completed`.
fn failed_outcome(record: &TaskRecord) -> bool {
    match &record.state {
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            !matches!(outcome, TaskOutcome::Completed { .. })
        }
        _ => false,
    }
}

/// The result carried by a terminal outcome.
fn outcome_result(record: &TaskRecord) -> JsonValue {
    match &record.state {
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => match outcome {
            TaskOutcome::Completed { result } => result.clone(),
            TaskOutcome::Failed { result, .. } | TaskOutcome::Aborted { result, .. } => {
                result.clone().unwrap_or(JsonValue::Null)
            }
            TaskOutcome::Orphaned { .. } | TaskOutcome::Faulted { .. } => JsonValue::Null,
        },
        _ => JsonValue::Null,
    }
}

/// A terminal receipt from a record.
fn settled_task(record: TaskRecord) -> SettledTask<JsonValue> {
    let result = outcome_result(&record);
    SettledTask { record, result }
}

/// Convert a scheduler-written outcome into its Harness-cleanup form.
fn scheduler_outcome(outcome: &TaskOutcome) -> Option<SchedulerOutcome> {
    match outcome {
        TaskOutcome::Faulted { error } => Some(SchedulerOutcome::Faulted {
            error: error.clone(),
        }),
        TaskOutcome::Orphaned { reason } => Some(SchedulerOutcome::Orphaned {
            reason: reason.clone(),
        }),
        _ => None,
    }
}

/// The persisted reason string of a blocked task.
fn blocked_reason_str(reason: BlockedReason) -> &'static str {
    match reason {
        BlockedReason::MissingTask => "missing_task",
        BlockedReason::TaskTooOld => "task_too_old",
        BlockedReason::MigrationFailed => "migration_failed",
        BlockedReason::RestartLimit => "restart_limit",
    }
}

/// The ownership node of a record (upstream `nodeOf`).
fn node_of(record: &TaskRecord) -> TaskNode {
    TaskNode {
        conversation_id: record.conversation_id,
        owner: record.owner,
        background: record.background,
    }
}

/// The parent of a record's node (upstream `parentOf`).
fn parent_of(record: &TaskRecord) -> Up {
    match record.owner {
        Some(owner) => Up::Task(owner),
        None => Up::Conversation(record.conversation_id),
    }
}

/// The parent of a node (upstream `parentOf`).
fn parent_of_node(node: &TaskNode) -> Up {
    match node.owner {
        Some(owner) => Up::Task(owner),
        None => Up::Conversation(node.conversation_id),
    }
}

/// The staged overlay of one commit (upstream `overlayOf`).
fn overlay_of(tx: &Transaction) -> Overlay {
    let tasks = tx
        .staged_tasks()
        .into_iter()
        .map(|record| (record.id, record))
        .collect();
    let edges = tx
        .staged_conversations()
        .into_iter()
        .map(|record| (record.id, record.owner.as_ref().map(|owner| owner.task_id)))
        .collect();
    Overlay { tasks, edges }
}

/// Replace a live record's state; memos disappear once an outcome is decided.
/// Emit one report's fixed telemetry: the event carries only its kind's
/// attributes, and the counter only its type (upstream `safeReport`).
pub(crate) fn emit_report_telemetry(
    telemetry: &crate::chord::telemetry::TelemetryContext,
    info: &crate::harness::types::ReportInfo,
) {
    let mut attributes = JsonObject::new();
    attributes.insert(
        "type".to_string(),
        serde_json::to_value(info.kind).unwrap_or(JsonValue::Null),
    );
    // Fixed attributes per report kind (upstream `safeReport`): `reason` is one
    // of the schema's fixed values and only `task_faulted`/`task_outdated` carry
    // it; `host_error` carries only the task id, and
    // `session_failed`/`listener_error` carry no extra attribute.
    match info.kind {
        crate::harness::types::ReportKind::TaskFaulted
        | crate::harness::types::ReportKind::TaskOutdated => {
            if let Some(reason) = &info.reason {
                attributes.insert("reason".to_string(), JsonValue::from(reason.clone()));
            }
            if let Some(extension) = &info.extension {
                attributes.insert("extension".to_string(), JsonValue::from(extension.clone()));
            }
            if let Some(task) = info.task_id {
                attributes.insert("task.id".to_string(), JsonValue::from(task.get()));
            }
        }
        crate::harness::types::ReportKind::TaskParked
        | crate::harness::types::ReportKind::MigrationFailed
        | crate::harness::types::ReportKind::TaskError
        | crate::harness::types::ReportKind::ExtensionError => {
            if let Some(extension) = &info.extension {
                attributes.insert("extension".to_string(), JsonValue::from(extension.clone()));
            }
            if let Some(task) = info.task_id {
                attributes.insert("task.id".to_string(), JsonValue::from(task.get()));
            }
        }
        crate::harness::types::ReportKind::HostError => {
            if let Some(task) = info.task_id {
                attributes.insert("task.id".to_string(), JsonValue::from(task.get()));
            }
        }
        crate::harness::types::ReportKind::SessionFailed
        | crate::harness::types::ReportKind::ListenerError => {}
    }
    telemetry.add_event("pi.durable.report", attributes);
    telemetry.record_metric(
        "pi.durable.reports",
        1.0,
        serde_json::json!({ "type": info.kind })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
}

/// The record with an abort mark: `restart` for an abandonment; without a
/// reason, a request, which replaces `restart`. The restart count starts over:
/// it now counts cut-offs of the abort handler (upstream `withAbortMark`).
fn with_abort_mark(record: &TaskRecord, reason: Option<&str>) -> TaskRecord {
    let mut next = record.clone();
    next.abort_requested = true;
    next.abort_reason = reason.map(str::to_string);
    next.restarts = None;
    next
}

fn with_state(record: &TaskRecord, state: TaskState) -> TaskRecord {
    let mut next = record.clone();
    let terminal_like = matches!(
        state,
        TaskState::Terminal { .. } | TaskState::Completing { .. }
    );
    next.state = state;
    if terminal_like {
        next.memos = None;
        next.restarts = None;
    }
    next
}

/// The record without its restart count, as after a state the task committed.
fn progressed(record: &TaskRecord) -> TaskRecord {
    let mut next = record.clone();
    next.restarts = None;
    next
}

/// The checkpoint of a runnable record, or `null`.
fn record_checkpoint(record: &TaskRecord) -> JsonValue {
    match &record.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. }
        | TaskState::Sleeping { checkpoint, .. } => checkpoint.clone(),
        TaskState::Completing { .. } | TaskState::Terminal { .. } => JsonValue::Null,
    }
}

/// Own memo entry only; memo names such as `toString` must not resolve elsewhere.
fn memo_of(record: Option<&TaskRecord>, name: &str) -> Option<JsonValue> {
    let memos = record?.memos.as_ref()?;
    memos.get(name).cloned()
}

/// Whether a definition can take the task at reservation (upstream `canReserve`).
fn can_reserve(task: &AnyTask, record: &TaskRecord) -> bool {
    let version = task.version();
    version == record.version || (version > record.version && task.has_migration())
}

/// Structural equality of two JSON values (upstream `jsonEqual`).
fn json_equal(left: &JsonValue, right: &JsonValue) -> bool {
    match (left, right) {
        (JsonValue::Array(left), JsonValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| json_equal(left, right))
        }
        (JsonValue::Object(left), JsonValue::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .map(|other| json_equal(value, other))
                        .unwrap_or(false)
                })
        }
        _ => left == right,
    }
}
