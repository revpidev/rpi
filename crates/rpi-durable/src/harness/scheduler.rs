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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_ai::models::Models;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::chord::context::{Context, await_with_context, with_abort_signal};
use crate::chord::json::{JsonObject, JsonValue};
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
    /// Fixed-field report callback.
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
    controller: CancellationToken,
    /// Context passed to handlers; cancelled by `controller`.
    context: Context,
    /// Watches acquired through the runtime; stopped at invocation end.
    watches: Mutex<Vec<CommittedWatch>>,
    /// Timers armed through the runtime; disarmed at invocation end.
    timers: Mutex<Vec<TimerDisarm>>,
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
        if invocation.ended.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
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
        reported: Arc<Mutex<Option<AnyTask>>>,
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
            last_wake: Mutex::new(None),
            submissions: std::sync::OnceLock::new(),
            live: Mutex::new(BTreeMap::new()),
            contexts: Mutex::new(BTreeMap::new()),
            invocations: Mutex::new(BTreeMap::new()),
            task_waiters: Waiters::new(),
            idle_waiters: Waiters::new(),
            failed_migrations: Mutex::new(BTreeMap::new()),
            edges: Mutex::new(BTreeMap::new()),
            conversation_owners: Mutex::new(BTreeSet::new()),
            settled: Mutex::new(BTreeMap::new()),
            fail_fast_checks: Mutex::new(BTreeSet::new()),
            cascade_pending: AtomicBool::new(false),
            reconcile_scheduled: AtomicBool::new(false),
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
        Ok(resolve_agent(
            state.as_ref(),
            snapshot.as_ref(),
            &self.settings(),
            &self.report,
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
            .subscribe_internal_commits(Arc::new(move |publication, _| {
                observer.observe(publication);
            }));
        let sealer = Arc::clone(self);
        self.session
            .subscribe_close(Arc::new(move || sealer.seal()));
        let kicker = Arc::clone(self);
        let subscription = self.registry.subscribe(Arc::new(move || kicker.kick()));
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
                // A task its creator awaited only in memory is abandoned after a
                // restart: mark it `restart` so its abort handler runs instead of
                // a phase, and let the cascade reach its owned work.
                if record.abandon_on_restart && !record.abort_requested {
                    record.abort_requested = true;
                    record.abort_reason = Some("restart".to_string());
                    tx.set_task(record.clone())?;
                    self.cascade_pending.store(true, Ordering::SeqCst);
                }
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
                if let Ok(mut live) = self.live.lock() {
                    live.insert(record.id, record.clone());
                }
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
                .with_task(record.id, record.kind.clone())
                .with_reason("restart_limit"),
            );
        }
        Ok(())
    }

    /// The resolved restart limit.
    fn settings(&self) -> Settings {
        crate::harness::agent::resolve_settings(self.settings.current().as_ref())
    }

    fn restart_limit(&self) -> crate::harness::types::RestartLimit {
        self.settings().restart_limit
    }

    fn report_fixed(&self, info: crate::harness::types::ReportInfo) {
        self.telemetry.add_event(
            "pi.durable.report",
            serde_json::json!({
                "type": info.kind,
                "reason": info.reason,
                "task.id": info.task_id.map(|id| id.get()),
                "task.kind": info.task_kind,
                "extension": info.extension,
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        self.telemetry.record_metric("pi.durable.reports", 1.0);
        if let Some(report) = &self.report_info {
            report(info);
        }
    }

    /// Whether a record is parked by the restart limit.
    fn parked(&self, record: &TaskRecord) -> bool {
        !record.abort_requested
            && self
                .restart_limit()
                .limit()
                .map(|limit| record.restarts.unwrap_or(0) as u64 >= limit)
                .unwrap_or(false)
    }

    fn resume(self: &Arc<Self>) {
        self.enabled.store(true, Ordering::SeqCst);
        self.schedule_wake();
        self.kick();
    }

    /// When this Session next needs a running process.
    fn wake_snapshot(self: &Arc<Self>) -> Wake {
        let now = (self.now)();
        let mut active = self
            .invocations
            .lock()
            .map(|map| !map.is_empty())
            .unwrap_or(false);
        let mut at: Option<f64> = None;
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
            if record.abort_requested
                && !self.waiting_on(&record, &self.owned_live(None)).is_empty()
            {
                continue;
            }
            if !record.abort_requested {
                let snapshot = self.registry.snapshot();
                if matches!(
                    self.fit(&record, snapshot.task(&record.kind)),
                    Fit::Blocked { .. }
                ) {
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
        let wake = self.wake_snapshot();
        if let Some(on_wake) = self.on_wake.clone() {
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
            return;
        }
        self.arm_wake_timer(if self.enabled.load(Ordering::SeqCst) {
            wake.at
        } else {
            None
        });
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
                                let mut next = with_state(
                                    &record,
                                    TaskState::Pending {
                                        checkpoint: checkpoint.clone(),
                                    },
                                );
                                next.restarts = None;
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
            // The commit listener signalled the run; join it.
            let invocation = Arc::clone(run);
            if await_with_context(wait_invocation(&invocation), context)
                .await
                .is_err()
            {
                return Err(SessionError::Aborted);
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
        if !current.abort_requested {
            let mut marked = current.clone();
            marked.abort_requested = true;
            if !keep_restart {
                // An abort request replaces a restart mark.
                marked.abort_reason = None;
            }
            tx.set_task(marked)?;
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
                            if !record.abort_requested {
                                let mut marked = record.clone();
                                marked.abort_requested = true;
                                tx.set_task(marked)?;
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

    fn seal(self: &Arc<Self>) {
        self.closing.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = self.unsubscribe_registry.lock()
            && let Some(subscription) = slot.take()
        {
            subscription.unsubscribe();
        }
        self.task_waiters.reject_all(closed_error());
        self.idle_waiters.reject_all(closed_error());
        if let Ok(mut contexts) = self.contexts.lock() {
            contexts.clear();
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

    fn observe(self: &Arc<Self>, publication: &CommitPublication) {
        let mut updated: Vec<TaskRecord> = Vec::new();
        let mut failed: Vec<TaskId> = Vec::new();
        let mut changed = false;
        for change in &publication.changes {
            match change {
                CommitChange::Table(StorageWrite::Entry { value }) => {
                    self.telemetry.add_event(
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
                    self.telemetry.add_event(
                        "pi.durable.submission",
                        serde_json::json!({
                            "submission.id": value.id().get(),
                            "status": value.status(),
                        })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    );
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
                if let Ok(mut live) = self.live.lock() {
                    live.remove(&record.id);
                }
                if let Ok(mut map) = self.failed_migrations.lock() {
                    map.remove(&record.id);
                }
                if let Ok(mut set) = self.fail_fast_checks.lock() {
                    set.remove(&record.id);
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
        self.settle_idle();
        self.kick();
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
            if !self.closing.load(Ordering::SeqCst) {
                (self.report)(error.to_string());
            }
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
        let mut marked: BTreeSet<TaskId> = BTreeSet::new();
        let mut marks: Vec<TaskRecord> = Vec::new();
        let records: Vec<TaskRecord> = self
            .live
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        for record in records {
            if !record.background
                && self.below_cancelled(parent_of(&record))
                && !record.abort_requested
                && marked.insert(record.id)
            {
                marks.push(record);
            }
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
                    && !record.abort_requested
                    && marked.insert(record.id)
                {
                    marks.push(record);
                }
            }
        }
        for record in marks {
            let mut next = record.clone();
            next.abort_requested = true;
            tx.set_task(next)?;
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

    fn below_cancelled(self: &Arc<Self>, start: Up) -> bool {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return false,
                Step::Conversation(_) => continue,
                Step::Task(id, node) => {
                    let live = self
                        .live
                        .lock()
                        .ok()
                        .and_then(|live| live.get(&id).cloned());
                    if live
                        .map(|record| cancellation_intent(&record))
                        .unwrap_or(false)
                    {
                        return true;
                    }
                    if node.background {
                        return false;
                    }
                }
            }
        }
        false
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
        if let Err(error) = result
            && !self.closing.load(Ordering::SeqCst)
        {
            (self.report)(error.to_string());
        }
        self.draining.store(false, Ordering::SeqCst);
        // A wakeup that arrived during a failed pass still needs its pass.
        if self.dirty.load(Ordering::SeqCst) {
            self.kick();
        }
    }

    async fn reserve(self: &Arc<Self>) -> SessionResult<Vec<Reservation>> {
        let out: Arc<Mutex<Vec<Reservation>>> = Arc::new(Mutex::new(Vec::new()));
        let this = Arc::clone(self);
        let out_commit = Arc::clone(&out);
        let result = self
            .session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    let out = Arc::clone(&out_commit);
                    Box::pin(async move { this.reserve_in(tx, &out).await })
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
            Ok(()) => Ok(reservations),
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
                            message,
                            detail: Some(serde_json::json!({ "reason": "restart_limit" })),
                        },
                    },
                )
                .await?;
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
                    if mode == InvocationMode::Abort {
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
        if matches!(
            fit,
            Fit::Blocked {
                reason: BlockedReason::TaskTooOld,
                ..
            }
        ) {
            self.report_fixed(
                crate::harness::types::ReportInfo::new(
                    crate::harness::types::ReportKind::TaskOutdated,
                )
                .with_task(record.id, record.kind.clone())
                .with_reason("task_too_old"),
            );
        }
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
                            .with_task(record.id, record.kind.clone())
                            .with_reason("migration_failed"),
                        );
                        Resolution::Blocked {
                            reason: BlockedReason::MigrationFailed,
                        }
                    }
                }
            }
        }
    }

    fn fit(self: &Arc<Self>, record: &TaskRecord, task: Option<AnyTask>) -> Fit {
        let Some(task) = task else {
            return Fit::Blocked {
                reason: BlockedReason::MissingTask,
                error: None,
            };
        };
        let version = task.version();
        if version == record.version {
            return Fit::Ready {
                task,
                migrates: false,
            };
        }
        if version < record.version {
            return Fit::Blocked {
                reason: BlockedReason::TaskTooOld,
                error: None,
            };
        }
        let failed = self
            .failed_migrations
            .lock()
            .ok()
            .and_then(|map| map.get(&record.id).cloned());
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
        let invocation = Arc::new(Invocation {
            task_id: record.id,
            conversation_id: record.conversation_id,
            mode,
            controller,
            context,
            watches: Mutex::new(Vec::new()),
            timers: Mutex::new(Vec::new()),
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
        self.telemetry.record_metric("pi.durable.tasks.active", 1.0);
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
        let reported: Arc<Mutex<Option<AnyTask>>> = Arc::new(Mutex::new(None));
        let mut previous: Option<PhaseResult> = None;
        loop {
            let decider = Decider::Run {
                previous: previous.clone(),
                task: Arc::clone(&task),
                snapshot: Arc::clone(&snapshot_cell),
                reported: Arc::clone(&reported),
            };
            let current = self.step(&invocation, decider).await?;
            let Some(current) = current else {
                return Ok(());
            };
            // Close may seal between the decision and dispatch.
            if self.closing.load(Ordering::SeqCst) {
                return Ok(());
            }
            let phase = current.phase().unwrap_or("").to_string();
            let checkpoint = current.checkpoint().cloned().unwrap_or(JsonValue::Null);
            let runtime = InvocationRuntime {
                inner: Arc::clone(self),
                invocation: Arc::clone(&invocation),
                snapshot: Arc::clone(&snapshot_cell),
                agent: Arc::new(tokio::sync::OnceCell::new()),
            };
            if runtime.inner.telemetry.enabled() {
                runtime.inner.telemetry.set_attributes(
                    serde_json::json!({
                        "task.id": invocation.task_id.get(),
                        "task.name": task.name(),
                        "phase": phase,
                        "kind": if invocation.mode == InvocationMode::Abort { "abort" } else { "run" },
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                );
            }
            let started = (self.now)();
            let outcome = task
                .run_phase(&phase, current, &runtime, &invocation.context)
                .await;
            if runtime.inner.telemetry.enabled() {
                runtime.inner.telemetry.record_metric(
                    "pi.durable.phase.duration",
                    ((self.now)() - started).max(0.0),
                );
            }
            previous = Some(PhaseResult {
                checkpoint,
                phase,
                failure: outcome.err().map(|error| error.to_string()),
            });
        }
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
        let runtime = InvocationRuntime {
            inner: Arc::clone(self),
            invocation: Arc::clone(invocation),
            snapshot: Arc::new(Mutex::new(Arc::clone(&reservation.snapshot))),
            agent: Arc::new(tokio::sync::OnceCell::new()),
        };
        let failure = reservation
            .task
            .abort(current, &runtime, &invocation.context)
            .await
            .err()
            .map(|error| error.to_string());
        self.step(invocation, Decider::Abort { failure }).await?;
        Ok(())
    }

    fn decide(
        self: &Arc<Self>,
        decider: &Decider,
        tx: &mut Transaction,
        current: &RunningTask,
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
                        match (&*reported, &next) {
                            (Some(previous), Some(next)) => Arc::ptr_eq(previous, next),
                            (None, None) => true,
                            _ => false,
                        }
                    };
                    if !already {
                        if let Ok(mut reported) = reported.lock() {
                            *reported = next.clone();
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
    ) -> SessionResult<Option<RunningTask>> {
        let this = Arc::clone(self);
        let inv = Arc::clone(invocation);
        let result: SessionResult<Option<RunningTask>> = self
            .session
            .commit_with(
                commit_fn(move |tx| {
                    let this = Arc::clone(&this);
                    let inv = Arc::clone(&inv);
                    let decider = decider.clone();
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
                                this.decide(&decider, tx, current)?
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
                            this.report_fixed(
                                crate::harness::types::ReportInfo::new(
                                    crate::harness::types::ReportKind::TaskFaulted,
                                )
                                .with_task(current.id(), current.kind().to_string()),
                            );
                            this.terminate(
                                tx,
                                &current.record,
                                TaskOutcome::Faulted {
                                    error: TaskOutcomeError {
                                        message,
                                        detail: None,
                                    },
                                },
                            )
                            .await?;
                        }
                        Ok(None)
                    })
                }),
                &self.context,
                None,
            )
            .await;
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.end(invocation);
                if !self.closing.load(Ordering::SeqCst) {
                    (self.report)(error.to_string());
                }
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
        if let NextTaskState::Waiting { on, policy, .. } = &next {
            self.validate_wait(tx, invocation, current, on, *policy)
                .await?;
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
        self.telemetry
            .record_metric("pi.durable.tasks.active", -1.0);
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
        if let Ok(mut timers) = invocation.timers.lock() {
            for disarm in timers.drain(..) {
                disarm();
            }
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
        if let TaskState::Sleeping { until, .. } = &record.state {
            return TaskInspectionState::Sleeping { until: *until };
        }
        if self.parked(record) {
            return TaskInspectionState::Blocked {
                reason: BlockedReason::RestartLimit,
                error: None,
            };
        }
        let on = self.waiting_on(record, owned);
        if !on.is_empty() {
            return TaskInspectionState::Waiting { on };
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
        let wrapper: Arc<GatedFn<()>> = Arc::new(move |tx, current| {
            let change = Arc::clone(&change);
            let inner = Arc::clone(&inner);
            let invocation = Arc::clone(&invocation);
            Box::pin(async move {
                let next = change(tx, current.clone()).await?;
                if let Some(next) = next {
                    inner.commit_state(tx, &invocation, &current, next).await?;
                }
                Ok(())
            })
        });
        self.gated(wrapper, context).await
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

    async fn owned_tasks(&self, _context: &Context) -> SessionResult<Vec<TaskRecord>> {
        self.ensure_open()?;
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
        let owner = self
            .inner
            .live
            .lock()
            .ok()
            .and_then(|live| live.get(&id).cloned())
            .and_then(|record| record.owner);
        if owner != Some(self.invocation.task_id) {
            return Err(SessionError::message(format!(
                "Task {id} is not owned by task {}",
                self.invocation.task_id
            )));
        }
        let _ = self.inner.abort(id, context, true).await?;
        let bound = with_abort_signal(self.invocation.controller.clone(), context);
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

    fn set_timer(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        delay_ms: u64,
    ) -> SessionResult<TimerDisarm> {
        self.ensure_open()?;
        let callback = Arc::new(move || {
            // After the invocation ends, nothing is armed; a race is harmless.
            callback();
        });
        let disarm = self.inner.timers.foreground(callback, delay_ms as f64);
        if let Ok(mut timers) = self.invocation.timers.lock() {
            timers.push(Arc::clone(&disarm));
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
        let inner = Arc::clone(&scheduler.inner);
        let session = Arc::clone(&inner.session);
        session.on_failure(Arc::new(move |cause| {
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

/// A live owner's durable cancellation intent (upstream `cancellationIntent`).
fn cancellation_intent(record: &TaskRecord) -> bool {
    record.status() != TaskStatus::Terminal && (record.abort_requested || failed_outcome(record))
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
