//! Session kernel: one mutation line, the loaded document tracker cache, and
//! committed publication (port of `packages/durable/src/session/session.ts`
//! @ `b4afdddf`).
//!
//! Only committed state is observable. Commit callbacks and settlement run on
//! the single mutation line; the Writer stores admitted batches off the line
//! and publishes them in admission order. Listeners run synchronously during
//! that publication and must not re-enter.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use futures::FutureExt;

use crate::chord::context::{AbortSignal, Context, without_abort_signal};
use crate::chord::json::{JsonObject, JsonValue};
use crate::chord::tracker::track;
use crate::documents::{
    DocScope, DocTarget, DocToken, check_record_scope_record, check_record_version_record,
    materialize_document, resolve_address,
};
use crate::harness::types::{ReportFn, SetTimer, TimerDisarm};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq};
use crate::session::gated::GatedStorage;
use crate::session::guarded::GuardedStorage;
use crate::session::keys::document_key;
use crate::session::observation::{
    CommittedStateSource, CommittedWatch, WatchListener, attach_state,
};
use crate::session::transaction::{LoadedDocument, Transaction, TransactionScope};
use crate::session::writer::{CommitJob, JobOutcome, PublishFailure, Writer};
use crate::session::{
    CommitChange, CommitOptions, CommitPersistence, CommitPublication, DeferredCommit, DocOwner,
    SessionEnd, SessionError, SessionFuture, SessionResult, Subscription, doc_target,
    observed_operations, observed_operations_for,
};
use crate::timers::Timers;
use crate::types::{ConversationRecord, DocumentAddress, DocumentRecord, DocumentScope, Storage};

tokio::task_local! {
    /// Set while a fenced job runs on the mutation line in this task.
    static ON_LINE: ();
}

/// Wall clock used for task lifecycle times (upstream `Date.now`).
pub type NowFn = Arc<dyn Fn() -> f64 + Send + Sync>;

struct DocumentSweep {
    id: u64,
    at: f64,
    disarm: Option<TimerDisarm>,
}

struct FailureState {
    cause: String,
    at: u64,
    own: std::collections::HashSet<u64>,
}

#[derive(Clone)]
enum CloseError {
    Storage(String),
    Failed(String),
    Closed,
    Message(String),
}

impl CloseError {
    fn from_session_error(error: &SessionError) -> Self {
        match error {
            SessionError::Storage(_) => Self::Storage(error.to_string()),
            SessionError::Failed { cause } => Self::Failed(cause.clone()),
            SessionError::Closed => Self::Closed,
            _ => Self::Message(error.to_string()),
        }
    }

    fn into_session_error(self) -> SessionError {
        match self {
            Self::Storage(error) => {
                SessionError::Storage(crate::errors::StorageError::message(error))
            }
            Self::Failed(cause) => SessionError::Failed { cause },
            Self::Closed => SessionError::Closed,
            Self::Message(error) => SessionError::Message(error),
        }
    }
}

type WriterOutcome = tokio::sync::oneshot::Receiver<SessionResult<()>>;
type SharedWriterOutcome = Arc<tokio::sync::Mutex<Option<WriterOutcome>>>;

struct CurrentJobGuard {
    session: Arc<SessionImpl>,
    index: u64,
}

impl CurrentJobGuard {
    fn new(session: &Arc<SessionImpl>, index: u64, outcome: &SharedWriterOutcome) -> Self {
        if let Ok(mut current) = session.current_job.lock() {
            *current = Some((index, Arc::clone(outcome)));
        }
        Self {
            session: Arc::clone(session),
            index,
        }
    }
}

impl Drop for CurrentJobGuard {
    fn drop(&mut self) {
        if let Ok(mut current) = self.session.current_job.lock()
            && current.as_ref().map(|(index, _)| *index) == Some(self.index)
        {
            *current = None;
        }
    }
}

struct LineJob {
    session: Arc<SessionImpl>,
    index: u64,
    outcome: SharedWriterOutcome,
    delivered: bool,
    maintenance: bool,
}

impl LineJob {
    fn new(session: &Arc<SessionImpl>, index: u64) -> Self {
        Self {
            session: Arc::clone(session),
            index,
            outcome: Arc::new(tokio::sync::Mutex::new(None)),
            delivered: false,
            maintenance: false,
        }
    }

    fn maintenance(session: &Arc<SessionImpl>, index: u64) -> Self {
        let mut job = Self::new(session, index);
        job.maintenance = true;
        job
    }

    fn track_outcome(&self, outcome: WriterOutcome) {
        *self.outcome.try_lock().expect("writer outcome slot") = Some(outcome);
    }

    async fn wait_writer(&mut self) -> SessionResult<()> {
        let mut outcome = self.outcome.lock().await;
        let result = outcome
            .as_mut()
            .expect("writer outcome")
            .await
            .map_err(|_| SessionError::Closed)?;
        *outcome = None;
        result
    }

    async fn deliver<T: Send>(mut self, result: SessionResult<T>) -> SessionResult<T> {
        let result = if self.maintenance {
            self.session.deliver_maintenance(self.index, result).await
        } else {
            self.session.deliver(self.index, result).await
        };
        self.delivered = true;
        result
    }
}

impl Drop for LineJob {
    fn drop(&mut self) {
        if self.delivered {
            return;
        }
        let session = Arc::clone(&self.session);
        let index = self.index;
        let outcome = Arc::clone(&self.outcome);
        let maintenance = self.maintenance;
        tokio::spawn(async move {
            let outcome = outcome.lock().await.take();
            if let Some(outcome) = outcome {
                let _ = outcome.await;
            }
            if maintenance {
                let _ = session
                    .deliver_maintenance(index, Err::<(), _>(SessionError::Aborted))
                    .await;
            } else {
                let _ = session
                    .deliver(index, Err::<(), _>(SessionError::Aborted))
                    .await;
            }
        });
    }
}

/// Loaded-document cache counts (upstream `documentCacheSizes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentCacheSizes {
    pub cached: usize,
    pub idle: usize,
    pub attached: usize,
    pub sweeps: u64,
}

/// A synchronous post-adoption commit listener. It must not throw, block, or
/// call session operations (upstream `subscribeCommits`).
pub type CommitListener = Arc<dyn Fn(&CommitPublication, &Context) + Send + Sync>;

type CommitObserverRegistration = (
    u64,
    Arc<dyn crate::session::CommitObserver>,
    Arc<AtomicBool>,
);

/// A synchronous close listener, called once when close begins (upstream
/// `subscribeClose`).
pub type CloseListener = Arc<dyn Fn() + Send + Sync>;

/// One commit callback: receives the concrete transaction and returns a value.
pub type CommitFn<T> =
    Arc<dyn for<'a> Fn(&'a mut Transaction) -> SessionFuture<'a, T> + Send + Sync>;

/// A hook staged into every transaction that creates or forks a conversation.
pub type ConversationCreatedHook = Arc<
    dyn for<'a, 'r> Fn(&'a mut Transaction, &'r ConversationRecord) -> SessionFuture<'a, ()>
        + Send
        + Sync,
>;

/// Wrap a closure as a [`CommitFn`].
pub fn commit_fn<T, F>(function: F) -> CommitFn<T>
where
    F: for<'a> Fn(&'a mut Transaction) -> SessionFuture<'a, T> + Send + Sync + 'static,
{
    Arc::new(function)
}

static NEXT_LISTENER_ID: AtomicU64 = AtomicU64::new(1);

fn error_future<'a, T>(error: SessionError) -> SessionFuture<'a, T> {
    Box::pin(async move { Err(error) })
}

/// Reset an atomic flag when dropped unless disarmed. Close paths use this so a
/// cancelled await leaves the resource retryable.
struct ResetOnDrop<'a> {
    flag: &'a AtomicBool,
    armed: bool,
}

impl<'a> ResetOnDrop<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        Self { flag, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ResetOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(false, Ordering::SeqCst);
        }
    }
}

/// A conversation document's current incarnation and value, resolved on the line
/// (upstream `conversation_document_on_line`).
#[derive(Debug, Clone)]
pub struct ConversationDocumentView {
    /// Current incarnation record.
    pub record: DocumentRecord,
    /// Definition version of `value`.
    pub version: u64,
    /// Committed value.
    pub value: JsonObject,
}

/// Session kernel (upstream `SessionImpl`). Always shared as `Arc<SessionImpl>`.
pub struct SessionImpl {
    read_only: bool,
    raw: Arc<dyn Storage>,
    guarded: Arc<dyn Storage>,
    guarded_typed: Arc<GuardedStorage>,
    gated: OnceLock<Arc<dyn Storage>>,
    writer: OnceLock<Arc<Writer>>,
    now: NowFn,
    documents: Mutex<HashMap<String, Arc<LoadedDocument>>>,
    /// Commit listeners: `true` marks the Session's own component listener,
    /// whose throw fails the Session; host listeners report only.
    commit_listeners: Arc<Mutex<Vec<(u64, CommitListener, bool)>>>,
    commit_observers: Arc<Mutex<Vec<CommitObserverRegistration>>>,
    close_listeners: Arc<Mutex<Vec<(u64, CloseListener)>>>,
    conversation_created_hook: Mutex<Option<ConversationCreatedHook>>,
    line: tokio::sync::Mutex<()>,
    closing: AtomicBool,
    cleanup_done: AtomicBool,
    close_error: Mutex<Option<CloseError>>,
    /// The first error that failed the Session and its outcome boundary.
    failure: Mutex<Option<FailureState>>,
    current_job: Mutex<Option<(u64, SharedWriterOutcome)>>,
    next_job_index: AtomicU64,
    completed_jobs: Mutex<std::collections::BTreeSet<u64>>,
    outcome_delivered: tokio::sync::watch::Sender<u64>,
    /// The terminal session state, once close or failure set it.
    end: Mutex<Option<SessionEnd>>,
    end_notify: tokio::sync::Notify,
    failure_reporter: Mutex<Option<FailureReporter>>,
    /// Reports listener and close-listener panics; never fails the Session.
    listener_reporter: Mutex<Option<FailureReporter>>,
    /// Callbacks run when the Session fails, before close listeners.
    failure_hooks: Mutex<Vec<FailureHook>>,
    /// Last use of each cached document, by address, for retention.
    idle_since: Mutex<HashMap<String, f64>>,
    /// Incarnations with attached watches/states and how many; never evicted
    /// while the count is positive (upstream `#attachedDocuments`).
    attached: Mutex<std::collections::HashMap<DocumentId, usize>>,
    /// `settings.documentRetentionMs`, read at each use.
    document_retention_ms: Mutex<Option<DocumentRetentionReader>>,
    document_timers: Mutex<Arc<Timers>>,
    document_sweep: Mutex<Option<DocumentSweep>>,
    next_sweep_id: AtomicU64,
    sweep_jobs: AtomicU64,
    document_timers_unavailable: AtomicBool,
    deferred: Mutex<Option<DeferredCommit>>,
    deferred_pending: AtomicBool,
    /// Documents pinned by an unflushed deferred commit.
    pinned: Mutex<std::collections::HashSet<DocumentId>>,
    outstanding_commits: tokio::sync::Semaphore,
    next_deferred_id: AtomicU64,
    persist_within_ms: Mutex<Option<f64>>,
}

/// Reads `settings.documentRetentionMs` at each use.
pub type DocumentRetentionReader = Arc<dyn Fn() -> Option<f64> + Send + Sync>;

/// Host reporter for a Session failure or listener error and its optional telemetry context.
pub type FailureReporter = Arc<dyn Fn(String, Option<Context>) + Send + Sync>;
/// Callback run when the Session fails.
pub type FailureHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Open a session kernel over one storage backend (upstream `createSession`).
pub fn create_session(storage: Arc<dyn Storage>, now: Option<NowFn>) -> Arc<SessionImpl> {
    SessionImpl::new(storage, now)
}

impl SessionImpl {
    /// Build a session over `storage` with the given clock (default [`crate::session::default_now`]).
    pub fn new(storage: Arc<dyn Storage>, now: Option<NowFn>) -> Arc<Self> {
        Self::new_with_options(storage, now, false)
    }

    /// Build a session with explicit read-only admission.
    pub fn new_with_options(
        storage: Arc<dyn Storage>,
        now: Option<NowFn>,
        read_only: bool,
    ) -> Arc<Self> {
        let guarded_typed = GuardedStorage::new(Arc::clone(&storage));
        let guarded: Arc<dyn Storage> = guarded_typed.clone();
        let session = Arc::new(Self {
            read_only,
            raw: Arc::clone(&storage),
            guarded,
            guarded_typed,
            gated: OnceLock::new(),
            writer: OnceLock::new(),
            now: now.unwrap_or_else(|| Arc::new(crate::session::default_now)),
            documents: Mutex::new(HashMap::new()),
            commit_listeners: Arc::new(Mutex::new(Vec::new())),
            commit_observers: Arc::new(Mutex::new(Vec::new())),
            close_listeners: Arc::new(Mutex::new(Vec::new())),
            conversation_created_hook: Mutex::new(None),
            line: tokio::sync::Mutex::new(()),
            closing: AtomicBool::new(false),
            cleanup_done: AtomicBool::new(false),
            close_error: Mutex::new(None),
            failure: Mutex::new(None),
            current_job: Mutex::new(None),
            next_job_index: AtomicU64::new(1),
            completed_jobs: Mutex::new(std::collections::BTreeSet::new()),
            outcome_delivered: tokio::sync::watch::channel(0).0,
            end: Mutex::new(None),
            end_notify: tokio::sync::Notify::new(),
            failure_reporter: Mutex::new(None),
            listener_reporter: Mutex::new(None),
            failure_hooks: Mutex::new(Vec::new()),
            idle_since: Mutex::new(HashMap::new()),
            attached: Mutex::new(std::collections::HashMap::new()),
            document_retention_ms: Mutex::new(None),
            document_timers: Mutex::new(Timers::new(None, Arc::new(|_| {}))),
            document_sweep: Mutex::new(None),
            next_sweep_id: AtomicU64::new(1),
            sweep_jobs: AtomicU64::new(0),
            document_timers_unavailable: AtomicBool::new(false),
            deferred: Mutex::new(None),
            deferred_pending: AtomicBool::new(false),
            pinned: Mutex::new(std::collections::HashSet::new()),
            outstanding_commits: tokio::sync::Semaphore::new(256),
            next_deferred_id: AtomicU64::new(1),
            persist_within_ms: Mutex::new(Some(1000.0)),
        });
        session.guarded_typed.attach(&session);
        let weak = Arc::downgrade(&session);
        let writer = Writer::new(Arc::clone(&session.guarded), weak.clone());
        let gated = GatedStorage::new(Arc::clone(&session.guarded), weak);
        let _ = session.writer.set(writer);
        let _ = session.gated.set(gated);
        session
    }

    /// The storage backend used by line jobs, with failure and read gates.
    pub fn storage(&self) -> &Arc<dyn Storage> {
        self.gated.get().expect("session gated storage")
    }

    fn writer(&self) -> &Arc<Writer> {
        self.writer.get().expect("session writer")
    }

    pub(crate) async fn wait_for_writer(&self, key: &str) -> SessionResult<()> {
        self.writer().wait_key(key).await;
        self.assert_healthy()
    }

    pub(crate) async fn wait_for_all_writers(&self) -> SessionResult<()> {
        self.wait_for_writer(crate::session::keys::ALL_KEYS).await
    }

    /// The raw storage backend, for closing and tests.
    pub fn raw_storage(&self) -> &Arc<dyn Storage> {
        &self.raw
    }

    /// Whether this session rejects all writes before Storage.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Commit outcomes admitted but not delivered yet (bounded to 256).
    pub fn outstanding_commit_count(&self) -> usize {
        256 - self.outstanding_commits.available_permits()
    }

    /// Document writes still held for deferred persistence.
    pub fn deferred_write_count(&self) -> usize {
        self.deferred
            .lock()
            .ok()
            .and_then(|deferred| deferred.as_ref().map(|deferred| deferred.writes.len()))
            .unwrap_or(0)
    }

    /// The first error that failed the Session, if one did.
    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .ok()
            .and_then(|failure| failure.as_ref().map(|failure| failure.cause.clone()))
    }

    /// Register a callback run when the Session fails; it rejects the
    /// caller's pending waits.
    pub fn on_failure(&self, hook: FailureHook) {
        if let Ok(mut hooks) = self.failure_hooks.lock() {
            hooks.push(hook);
        }
    }

    /// Install the host reporter for the error that failed the Session.
    pub fn set_failure_reporter(&self, reporter: FailureReporter) {
        if let Ok(mut slot) = self.failure_reporter.lock() {
            *slot = Some(reporter);
        }
    }

    /// Install the reporter for listener and close-listener panics.
    pub fn set_listener_reporter(&self, reporter: FailureReporter) {
        if let Ok(mut slot) = self.listener_reporter.lock() {
            *slot = Some(reporter);
        }
    }

    pub(crate) fn report_listener_error(&self, message: String, context: Option<Context>) {
        if let Ok(slot) = self.listener_reporter.lock()
            && let Some(reporter) = slot.as_ref()
        {
            reporter(message, context);
        }
    }

    /// Fail the Session: the first failure wins. Admission ends, the close
    /// listeners run at once, and Storage closes in the background. Later
    /// calls and waits reject with `SessionFailed`.
    pub fn fail(self: &Arc<Self>, cause: impl Into<String>) {
        self.fail_with_context(cause, None);
    }

    pub(crate) fn fail_at(self: &Arc<Self>, cause: String, at: u64, own: Vec<u64>) {
        self.fail_with_boundary(cause, None, Some(at), own);
    }

    pub(crate) fn fail_with_context(
        self: &Arc<Self>,
        cause: impl Into<String>,
        context: Option<&Context>,
    ) {
        self.fail_with_boundary(cause.into(), context, None, Vec::new());
    }

    fn fail_with_boundary(
        self: &Arc<Self>,
        cause: String,
        context: Option<&Context>,
        at: Option<u64>,
        own: Vec<u64>,
    ) {
        let at = at.unwrap_or_else(|| {
            self.current_job
                .lock()
                .ok()
                .and_then(|current| current.as_ref().map(|(index, _)| *index))
                .unwrap_or_else(|| *self.outcome_delivered.borrow())
        });
        {
            let Ok(mut failure) = self.failure.lock() else {
                return;
            };
            if failure.is_some() {
                return;
            }
            *failure = Some(FailureState {
                cause: cause.clone(),
                at,
                own: own.into_iter().collect(),
            });
        }
        self.closing.store(true, Ordering::SeqCst);
        let hooks: Vec<FailureHook> = match self.failure_hooks.lock() {
            Ok(hooks) => hooks.clone(),
            Err(_) => Vec::new(),
        };
        for hook in hooks {
            hook(cause.as_str());
        }
        self.notify_close();
        if let Ok(reporter) = self.failure_reporter.lock()
            && let Some(reporter) = reporter.as_ref()
        {
            reporter(cause.clone(), context.cloned());
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let this = Arc::clone(self);
            handle.spawn(async move {
                let _ = this.close(&crate::chord::context::BACKGROUND_CONTEXT).await;
            });
        } else {
            // Without a runtime no close can run; settle the terminal state so
            // `closed()` cannot hang forever.
            self.set_end(SessionEnd::Failed {
                error: cause.clone(),
            });
        }
    }

    /// Resolve with the terminal session state once close or failure reaches it.
    pub async fn closed(&self) -> SessionEnd {
        loop {
            let notified = self.end_notify.notified();
            tokio::pin!(notified);
            // Register before checking: `notify_waiters` stores no permit.
            notified.as_mut().enable();
            if let Some(end) = self.end.lock().ok().and_then(|end| end.clone()) {
                return end;
            }
            notified.await;
        }
    }

    fn set_end(&self, end: SessionEnd) {
        if let Ok(mut slot) = self.end.lock()
            && slot.is_none()
        {
            *slot = Some(end);
        }
        self.end_notify.notify_waiters();
    }

    fn notify_close(&self) {
        let listeners: Vec<CloseListener> = match self.close_listeners.lock() {
            Ok(mut listeners) => listeners.drain(..).map(|(_, listener)| listener).collect(),
            Err(_) => Vec::new(),
        };
        for listener in listeners {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener())).is_err()
                && let Ok(reporter) = self.listener_reporter.lock()
                && let Some(reporter) = reporter.as_ref()
            {
                reporter("A close listener panicked".to_string(), None);
            }
        }
    }

    /// Current wall clock in milliseconds.
    pub fn now(&self) -> f64 {
        (self.now)()
    }

    /// Install a conversation-created hook (base sessions stage nothing).
    pub fn set_conversation_created_hook(&self, hook: ConversationCreatedHook) {
        if let Ok(mut slot) = self.conversation_created_hook.lock() {
            *slot = Some(hook);
        }
    }

    pub(crate) fn cached(self: &Arc<Self>, address_id: &str) -> Option<Arc<LoadedDocument>> {
        let document = self
            .documents
            .lock()
            .ok()
            .and_then(|documents| documents.get(address_id).cloned());
        if document.is_some() {
            self.touch(address_id);
        }
        document
    }

    /// Whether the caller is already running a fenced job on the mutation line.
    fn is_on_line(&self) -> bool {
        ON_LINE.try_with(|_| ()).is_ok()
    }

    /// Set the default deferred persistence deadline; `None` means no timer.
    pub fn set_persist_within_ms(&self, within_ms: Option<f64>) {
        if let Ok(mut slot) = self.persist_within_ms.lock() {
            *slot = within_ms;
        }
    }

    /// Install the live `documentRetentionMs` reader.
    pub fn set_document_retention(&self, retention: DocumentRetentionReader) {
        if let Ok(mut slot) = self.document_retention_ms.lock() {
            *slot = Some(retention);
        }
    }

    pub fn set_document_timer(&self, set_timer: Option<SetTimer>, report: ReportFn) {
        if let Ok(mut timers) = self.document_timers.lock() {
            *timers = Timers::new(set_timer, report);
        }
    }

    /// Cache counts for diagnostics and retention conformance.
    pub fn document_cache_sizes(&self) -> DocumentCacheSizes {
        DocumentCacheSizes {
            cached: self
                .documents
                .lock()
                .map(|documents| documents.len())
                .unwrap_or(0),
            idle: self.idle_since.lock().map(|idle| idle.len()).unwrap_or(0),
            attached: self
                .attached
                .lock()
                .map(|attached| attached.len())
                .unwrap_or(0),
            sweeps: self.sweep_jobs.load(Ordering::SeqCst),
        }
    }

    fn document_retention(&self) -> f64 {
        let reader = self
            .document_retention_ms
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        match reader.as_ref().and_then(|reader| reader()) {
            Some(ms) if ms == f64::INFINITY || (ms.is_finite() && ms >= 0.0) => ms,
            _ => 600_000.0,
        }
    }

    pub(crate) fn touch(self: &Arc<Self>, address_id: &str) {
        let Some(document) = self
            .documents
            .lock()
            .ok()
            .and_then(|documents| documents.get(address_id).cloned())
        else {
            return;
        };
        if self
            .attached
            .lock()
            .map(|attached| attached.contains_key(&document.record.id))
            .unwrap_or(false)
        {
            if let Ok(mut idle) = self.idle_since.lock() {
                idle.remove(address_id);
            }
            return;
        }
        let now = (self.now)();
        let retention = self.document_retention();
        if let Ok(mut idle) = self.idle_since.lock() {
            if idle
                .get(address_id)
                .is_some_and(|since| now - *since < (1_000.0_f64).min(retention / 10.0))
            {
                return;
            }
            idle.insert(address_id.to_string(), now);
        }
        self.schedule_eviction();
    }

    pub(crate) fn schedule_eviction(self: &Arc<Self>) {
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        let retention = self.document_retention();
        if !retention.is_finite() {
            return;
        }
        let attached = self
            .attached
            .lock()
            .map(|attached| attached.clone())
            .unwrap_or_default();
        let documents: HashMap<String, DocumentId> = self
            .documents
            .lock()
            .map(|documents| {
                documents
                    .iter()
                    .map(|(address, document)| (address.clone(), document.record.id))
                    .collect()
            })
            .unwrap_or_default();
        let oldest = self.idle_since.lock().ok().and_then(|idle| {
            idle.iter()
                .filter(|(address, _)| {
                    documents.get(*address).is_some_and(|id| {
                        !attached.contains_key(id) && !self.writer().has_key(&document_key(address))
                    })
                })
                .min_by(|(left_address, left), (right_address, right)| {
                    left.total_cmp(right)
                        .then_with(|| left_address.cmp(right_address))
                })
                .map(|(address, since)| (address.clone(), *since))
        });
        let Some((_, since)) = oldest else {
            return;
        };
        let at = since + retention;
        {
            let mut pending = match self.document_sweep.lock() {
                Ok(pending) => pending,
                Err(_) => return,
            };
            if pending.as_ref().is_some_and(|sweep| sweep.at <= at) {
                return;
            }
            if let Some(sweep) = pending.as_mut()
                && let Some(disarm) = sweep.disarm.take()
            {
                disarm();
            }
        }
        let id = self.next_sweep_id.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut pending) = self.document_sweep.lock() {
            *pending = Some(DocumentSweep {
                id,
                at,
                disarm: None,
            });
        }
        let delay = at - (self.now)();
        let run: Arc<dyn Fn() + Send + Sync> = {
            let this = Arc::clone(self);
            Arc::new(move || {
                let this = Arc::clone(&this);
                this.sweep_jobs.fetch_add(1, Ordering::SeqCst);
                let Ok(handle) = tokio::runtime::Handle::try_current() else {
                    if let Ok(mut pending) = this.document_sweep.lock()
                        && pending.as_ref().is_some_and(|sweep| sweep.id == id)
                    {
                        *pending = None;
                    }
                    return;
                };
                handle.spawn(async move {
                    let this_for_job = Arc::clone(&this);
                    let result = this
                        .read_on_line(move || {
                            let this = Arc::clone(&this_for_job);
                            Box::pin(async move {
                                let current = this
                                    .document_sweep
                                    .lock()
                                    .map(|mut pending| {
                                        if pending.as_ref().is_some_and(|sweep| sweep.id == id) {
                                            pending.take();
                                            true
                                        } else {
                                            false
                                        }
                                    })
                                    .unwrap_or(false);
                                if current && !this.closing.load(Ordering::SeqCst) {
                                    this.evict_idle();
                                }
                                Ok(())
                            })
                        })
                        .await;
                    let _ = result;
                });
            })
        };
        if delay <= 0.0 {
            run();
            return;
        }
        if self.document_timers_unavailable.load(Ordering::SeqCst) {
            if let Ok(mut pending) = self.document_sweep.lock() {
                *pending = None;
            }
            return;
        }
        let disarm = self
            .document_timers
            .lock()
            .map(|timers| timers.clone())
            .unwrap_or_else(|_| Timers::new(None, Arc::new(|_| {})))
            .background(run, delay);
        match disarm {
            Some(disarm) => {
                if let Ok(mut pending) = self.document_sweep.lock()
                    && let Some(sweep) = pending.as_mut()
                    && sweep.id == id
                {
                    sweep.disarm = Some(disarm);
                }
            }
            None => {
                self.document_timers_unavailable
                    .store(true, Ordering::SeqCst);
                if let Ok(mut pending) = self.document_sweep.lock()
                    && pending.as_ref().is_some_and(|sweep| sweep.id == id)
                {
                    *pending = None;
                }
            }
        }
    }

    /// Drop expired unattached documents on the mutation line, then arm the next sweep.
    fn evict_idle(self: &Arc<Self>) {
        let retention = self.document_retention();
        if retention.is_finite() {
            let expired_at = (self.now)() - retention;
            let attached = self
                .attached
                .lock()
                .map(|attached| attached.clone())
                .unwrap_or_default();
            let documents: HashMap<String, DocumentId> = self
                .documents
                .lock()
                .map(|documents| {
                    documents
                        .iter()
                        .map(|(address, document)| (address.clone(), document.record.id))
                        .collect()
                })
                .unwrap_or_default();
            let pinned = self
                .pinned
                .lock()
                .map(|pinned| pinned.clone())
                .unwrap_or_default();
            let mut expired: Vec<(String, f64)> = self
                .idle_since
                .lock()
                .map(|idle| {
                    idle.iter()
                        .filter(|(address, since)| {
                            documents.get(*address).is_some_and(|id| {
                                !attached.contains_key(id)
                                    && !pinned.contains(id)
                                    && !self.writer().has_key(&document_key(address))
                            }) && **since <= expired_at
                        })
                        .map(|(address, since)| (address.clone(), *since))
                        .collect()
                })
                .unwrap_or_default();
            expired.sort_by(|left, right| {
                left.1
                    .total_cmp(&right.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            if let Ok(mut documents) = self.documents.lock() {
                for (address, _) in &expired {
                    documents.remove(address);
                }
            }
            if let Ok(mut idle) = self.idle_since.lock() {
                for (address, _) in expired {
                    idle.remove(&address);
                }
            }
        }
        self.schedule_eviction();
    }

    pub(crate) fn attach_document(self: &Arc<Self>, address_id: &str, record_id: DocumentId) {
        if let Ok(mut attached) = self.attached.lock() {
            *attached.entry(record_id).or_insert(0) += 1;
        }
        if let Ok(mut idle) = self.idle_since.lock() {
            idle.remove(address_id);
        }
    }

    pub(crate) fn detach_document(self: &Arc<Self>, address_id: &str, record_id: DocumentId) {
        let mut released = false;
        if let Ok(mut attached) = self.attached.lock()
            && let Some(count) = attached.get_mut(&record_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                attached.remove(&record_id);
                released = true;
            }
        }
        if released
            && self
                .documents
                .lock()
                .ok()
                .and_then(|documents| documents.get(address_id).cloned())
                .is_some_and(|document| document.record.id == record_id)
        {
            self.touch(address_id);
        }
    }

    pub(crate) fn install(self: &Arc<Self>, document: Arc<LoadedDocument>) {
        let address = document.address_id.clone();
        if let Ok(mut documents) = self.documents.lock() {
            documents.insert(address.clone(), document);
        }
        self.touch(&address);
    }

    pub(crate) fn evict(&self, address_id: &str, record_id: DocumentId) {
        let removed = self
            .documents
            .lock()
            .map(|mut documents| {
                if documents
                    .get(address_id)
                    .map(|document| document.record.id == record_id)
                    .unwrap_or(false)
                {
                    documents.remove(address_id);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if removed && let Ok(mut idle) = self.idle_since.lock() {
            idle.remove(address_id);
        }
    }

    pub(crate) async fn load_document(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: &str,
        address: &DocumentAddress,
        context: &Context,
    ) -> SessionResult<Option<Arc<LoadedDocument>>> {
        self.wait_for_writer(&document_key(address_id)).await?;
        let cached = self.cached(address_id);
        if let Some(cached) = &cached
            && cached.value_version == definition.version
        {
            return Ok(Some(Arc::clone(cached)));
        }
        if let Some(cached) = &cached {
            self.flush_deferred_document_on_line(cached.record.id)
                .await?;
            if let Ok(mut documents) = self.documents.lock() {
                documents.remove(address_id);
            }
        }
        let record = self
            .storage()
            .find_document(
                address.clone(),
                crate::types::DocumentPoint::CURRENT,
                context,
            )
            .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        let stored = self
            .storage()
            .document(record.id, crate::types::DocumentPoint::CURRENT, context)
            .await?
            .ok_or_else(|| {
                SessionError::message(format!(
                    "Current document {} ({}) cannot be read",
                    record.id, record.kind
                ))
            })?;
        let value = materialize_document(definition, &stored)?;
        let loaded = Arc::new(LoadedDocument::new(
            address_id.to_string(),
            stored.record,
            stored.version,
            definition.version,
            stored.deltas_since_base,
            Arc::new(track(JsonValue::Object(value))),
        ));
        self.install(Arc::clone(&loaded));
        Ok(Some(loaded))
    }

    pub(crate) fn conversation_created<'a>(
        self: &'a Arc<Self>,
        tx: &'a mut Transaction,
        record: &'a ConversationRecord,
    ) -> SessionFuture<'a, ()> {
        let hook = self
            .conversation_created_hook
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        match hook {
            None => Box::pin(async { Ok(()) }),
            Some(hook) => hook(tx, record),
        }
    }

    // ─── Commit line ────────────────────────────────────────────────────────

    /// Commit one callback with an explicit default scope (upstream `commitWith`).
    pub fn commit_with<T: Send + 'static>(
        self: &Arc<Self>,
        change: CommitFn<T>,
        context: &Context,
        scope: Option<TransactionScope>,
    ) -> SessionFuture<'_, T> {
        if let Err(error) = self.assert_usable() {
            return error_future(error);
        }
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move {
            this.run_commit(change, context, scope, CommitOptions::default())
                .await
        })
    }

    /// Commit one callback with explicit persistence options.
    pub fn commit_with_options<T: Send + 'static>(
        self: &Arc<Self>,
        change: CommitFn<T>,
        context: &Context,
        scope: Option<TransactionScope>,
        options: CommitOptions,
    ) -> SessionFuture<'_, T> {
        if let Err(error) = self.assert_usable() {
            return error_future(error);
        }
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move { this.run_commit(change, context, scope, options).await })
    }

    /// Commit one callback (upstream `commit`).
    pub fn commit<T: Send + 'static>(
        self: &Arc<Self>,
        change: CommitFn<T>,
        context: &Context,
    ) -> SessionFuture<'_, T> {
        self.commit_with(change, context, None)
    }

    async fn run_commit<T: Send + 'static>(
        self: Arc<Self>,
        change: CommitFn<T>,
        context: Context,
        scope: Option<TransactionScope>,
        options: CommitOptions,
    ) -> SessionResult<T> {
        let _outstanding = self
            .outstanding_commits
            .acquire()
            .await
            .map_err(|_| SessionError::Closed)?;
        let guard = self.line.lock().await;
        let index = self.next_job_index.fetch_add(1, Ordering::SeqCst);
        let mut job = LineJob::new(&self, index);
        let result = std::panic::AssertUnwindSafe(ON_LINE.scope((), async {
            Arc::clone(&self)
                .run_commit_on_line(guard, &mut job, change, context, scope, options)
                .await
        }))
        .catch_unwind()
        .await
        .unwrap_or_else(|payload| {
            Err(SessionError::message(format!(
                "A commit callback panicked: {}",
                panic_message(payload)
            )))
        });
        job.deliver(result).await
    }

    async fn run_commit_on_line<T: Send>(
        self: Arc<Self>,
        _guard: tokio::sync::MutexGuard<'_, ()>,
        job: &mut LineJob,
        change: CommitFn<T>,
        context: Context,
        scope: Option<TransactionScope>,
        options: CommitOptions,
    ) -> SessionResult<T> {
        if self.read_only {
            return Err(SessionError::ReadOnlySession);
        }
        if let Some(CommitPersistence::Deferred {
            within_ms: Some(within_ms),
        }) = &options.persistence
            && (within_ms.is_nan() || *within_ms < 0.0)
        {
            return Err(SessionError::message(
                "CommitPersistence.within_ms must be a number of milliseconds, 0 or more",
            ));
        }
        let index = job.index;
        let _current = CurrentJobGuard::new(&self, index, &job.outcome);
        // Re-check close after waiting on the line: `commit_with` checked it
        // before the future was polled, but a concurrent close may have won
        // the line first. It must not run the callback or fail the Session.
        self.assert_usable()?;
        if context
            .abort_signal()
            .is_some_and(|signal| signal.is_cancelled())
        {
            return Err(SessionError::Aborted);
        }
        let persistence = options.persistence.unwrap_or(CommitPersistence::Immediate);
        let mut tx = Transaction::new(
            Arc::clone(&self),
            context.clone(),
            scope.unwrap_or_default(),
        );
        let result = match (change)(&mut tx).await {
            Ok(value) => value,
            Err(error) => {
                tx.settle_failure().await;
                self.evict_idle();
                return Err(error);
            }
        };
        // A callback may have swallowed a read error; the guard latched the
        // Session, so this commit must not commit or report success.
        if let Err(error) = self.assert_healthy() {
            tx.settle_failure().await;
            self.evict_idle();
            return Err(error);
        }
        let writes = match tx.settle_success().await {
            Ok(writes) => writes,
            Err(error) => {
                self.evict_idle();
                return Err(error);
            }
        };
        if writes.is_empty() {
            tx.discard();
            self.evict_idle();
            return Ok(result);
        }
        if let CommitPersistence::Deferred { within_ms } = persistence
            && tx.can_defer()
            && within_ms != Some(0.0)
        {
            let keys = tx.write_keys(&writes);
            let this = Arc::clone(&self);
            let job_context = context.clone();
            self.deferred_pending.store(true, Ordering::SeqCst);
            let outcome = self.writer().admit(CommitJob::held(
                index,
                keys,
                context.clone(),
                Box::new(move |outcome| {
                    let mut tx = tx;
                    match outcome {
                        JobOutcome::Held => this
                            .defer_commit(tx, writes, (), within_ms, &job_context)
                            .map(|_| ()),
                        JobOutcome::Discard => {
                            tx.discard();
                            let has_deferred = this
                                .deferred
                                .lock()
                                .ok()
                                .is_some_and(|deferred| deferred.is_some());
                            if !has_deferred {
                                this.deferred_pending.store(false, Ordering::SeqCst);
                            }
                            Ok(())
                        }
                        JobOutcome::Stored(_) => unreachable!(),
                    }
                }),
            ));
            job.track_outcome(outcome);
            self.evict_idle();
            drop(_current);
            drop(_guard);
            job.wait_writer().await?;
            return Ok(result);
        }
        let previous = self
            .deferred
            .lock()
            .ok()
            .and_then(|mut deferred| deferred.take());
        let previous_deadline = previous.as_ref().and_then(|held| held.deadline);
        let had_previous = previous.is_some();
        let (writes, remaining, taken_pins) = tx.take_held(previous);
        self.restore_deferred(had_previous, previous_deadline, remaining);
        let keys = tx.write_keys(&writes);
        let this = Arc::clone(&self);
        let job_context = context.clone();
        let published_writes = writes.clone();
        let outcome = self.writer().admit(CommitJob::stored(
            index,
            writes,
            keys,
            context.clone(),
            Box::new(move |outcome| {
                let mut tx = tx;
                match outcome {
                    JobOutcome::Stored(seq) => {
                        let documents = match tx.adopt(seq) {
                            Ok(documents) => documents,
                            Err(error) => {
                                tx.discard();
                                this.release_pins(&taken_pins);
                                return Err(PublishFailure::adoption(error));
                            }
                        };
                        let result = if taken_pins.is_empty() {
                            this.publish(seq, &published_writes, &documents, &job_context)
                        } else {
                            let (observed, stored) = tx.split_publications(documents);
                            this.publish_taken(
                                seq,
                                &published_writes,
                                &observed,
                                &stored,
                                &job_context,
                            )
                        };
                        this.release_pins(&taken_pins);
                        result
                    }
                    JobOutcome::Discard => {
                        tx.discard();
                        this.release_pins(&taken_pins);
                        Ok(())
                    }
                    JobOutcome::Held => unreachable!(),
                }
            }),
        ));
        job.track_outcome(outcome);
        self.evict_idle();
        drop(_current);
        drop(_guard);
        job.wait_writer().await?;
        Ok(result)
    }

    fn complete_job(&self, index: u64) {
        let Ok(mut completed) = self.completed_jobs.lock() else {
            return;
        };
        completed.insert(index);
        let mut next = *self.outcome_delivered.borrow() + 1;
        while completed.remove(&next) {
            next += 1;
        }
        let watermark = next - 1;
        let _ = self.outcome_delivered.send_replace(watermark);
    }

    async fn deliver_maintenance<T: Send>(
        &self,
        index: u64,
        result: SessionResult<T>,
    ) -> SessionResult<T> {
        self.complete_job(index);
        result
    }

    async fn deliver<T: Send>(
        self: &Arc<Self>,
        index: u64,
        result: SessionResult<T>,
    ) -> SessionResult<T> {
        let mut delivered = self.outcome_delivered.subscribe();
        while *delivered.borrow_and_update() < index - 1 {
            if delivered.changed().await.is_err() {
                return Err(SessionError::Closed);
            }
        }
        let poisoned = self.failure.lock().ok().and_then(|failure| {
            failure.as_ref().and_then(|failure| {
                (index > failure.at && !failure.own.contains(&index)).then(|| failure.cause.clone())
            })
        });
        let result = match poisoned {
            Some(cause) => Err(SessionError::Failed { cause }),
            None => result,
        };
        self.complete_job(index);
        result
    }

    fn defer_commit<T: Send>(
        self: &Arc<Self>,
        tx: Transaction,
        writes: Vec<crate::types::StorageWrite>,
        result: T,
        within_ms: Option<f64>,
        context: &Context,
    ) -> Result<T, PublishFailure> {
        let previous = self
            .deferred
            .lock()
            .ok()
            .and_then(|mut deferred| deferred.take());
        let previous_deadline = previous.as_ref().and_then(|held| held.deadline);
        let new_deadline = within_ms
            .or(self.persist_within_ms.lock().ok().and_then(|value| *value))
            .map(|within_ms| self.now() + within_ms);
        let id = previous
            .as_ref()
            .map(|held| held.id)
            .unwrap_or_else(|| self.next_deferred_id.fetch_add(1, Ordering::SeqCst));
        let (previous_writes, previous_changes, mut keys, mut pinned, mut deadlines, mut replaces) =
            match previous {
                Some(held) => (
                    Some(held.writes),
                    Some(held.changes),
                    held.keys,
                    held.pinned,
                    held.deadlines,
                    held.replaces,
                ),
                None => (
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    HashMap::new(),
                    std::collections::HashSet::new(),
                ),
            };
        for id in tx.document_ids() {
            if !pinned.contains(&id) {
                pinned.push(id);
            }
            let deadline = deadlines.entry(id).or_insert(new_deadline);
            *deadline = match (*deadline, new_deadline) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (Some(value), None) | (None, Some(value)) => Some(value),
                (None, None) => None,
            };
        }
        let deadline = crate::session::earliest_deferred_deadline(&deadlines);
        let arm_timer = deadline.is_some() && previous_deadline != deadline;
        for key in tx.write_keys(&writes) {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        let mut writes = merge_deferred_writes(previous_writes.as_ref(), writes);
        let current_changes = tx
            .adopt(crate::ids::Seq(0))
            .map_err(PublishFailure::adoption)?;
        let mut changes =
            merge_deferred_changes(previous_changes, current_changes.clone(), &replaces);
        compact_deferred_operations(&mut writes, &mut changes, &mut replaces);
        if let Ok(mut deferred) = self.deferred.lock() {
            *deferred = Some(DeferredCommit {
                id,
                deadline,
                deadlines,
                writes,
                changes,
                keys,
                pinned: pinned.clone(),
                replaces,
            });
        }
        if let Ok(mut set) = self.pinned.lock() {
            set.extend(pinned);
        }
        if arm_timer && let Some(deadline) = deadline {
            self.arm_deferred_timer(id, deadline);
        }
        self.publish_deferred(&current_changes, context)?;
        Ok(result)
    }

    fn restore_deferred(
        self: &Arc<Self>,
        had_previous: bool,
        previous_deadline: Option<f64>,
        remaining: Option<DeferredCommit>,
    ) {
        if !had_previous {
            return;
        }
        let mut timer = None;
        if let Some(mut held) = remaining {
            if held.deadline != previous_deadline {
                held.id = self.next_deferred_id.fetch_add(1, Ordering::SeqCst);
                timer = held.deadline.map(|deadline| (held.id, deadline));
            }
            self.deferred_pending.store(true, Ordering::SeqCst);
            if let Ok(mut deferred) = self.deferred.lock() {
                *deferred = Some(held);
            }
        } else {
            self.deferred_pending.store(false, Ordering::SeqCst);
        }
        if let Some((id, deadline)) = timer {
            self.arm_deferred_timer(id, deadline);
        }
    }

    fn arm_deferred_timer(self: &Arc<Self>, id: u64, deadline: f64) {
        if !deadline.is_finite() {
            return;
        }
        let weak = Arc::downgrade(self);
        let delay = (deadline - self.now()).max(0.0);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
            if let Some(session) = weak.upgrade() {
                let _guard = session.line.lock().await;
                if session.closing.load(Ordering::SeqCst) || session.failure().is_some() {
                    return;
                }
                if session
                    .deferred
                    .lock()
                    .ok()
                    .and_then(|deferred| deferred.as_ref().map(|held| held.id))
                    == Some(id)
                {
                    let _ = session.flush_deferred_on_line().await;
                }
            }
        });
    }

    async fn flush_deferred_document_on_line(
        self: &Arc<Self>,
        id: DocumentId,
    ) -> SessionResult<()> {
        self.flush_deferred_impl(Some(id)).await
    }

    pub(crate) async fn flush_deferred_on_line(self: &Arc<Self>) -> SessionResult<()> {
        self.flush_deferred_impl(None).await
    }

    async fn flush_deferred_impl(self: &Arc<Self>, only: Option<DocumentId>) -> SessionResult<()> {
        // A barrier even without holds, as upstream `#storeHolds` (and so
        // `unloadDocuments`): a commit admitted before this job must publish
        // before the cache is cleared, or its adoption would re-install a
        // document after the unload.
        self.writer().drained().await;
        if !self.deferred_pending.load(Ordering::SeqCst) {
            return Ok(());
        }
        let previous = self
            .deferred
            .lock()
            .ok()
            .and_then(|mut deferred| deferred.take());
        let had_previous = previous.is_some();
        let previous_deadline = previous.as_ref().and_then(|held| held.deadline);
        let (held, remaining) = match previous {
            Some(held) => split_deferred(held, only),
            None => return Ok(()),
        };
        self.restore_deferred(had_previous, previous_deadline, remaining);
        self.assert_healthy()?;
        let Some(held) = held else {
            return Ok(());
        };
        let this = Arc::clone(self);
        let writes = held.writes.clone();
        let current_index = self
            .current_job
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|(index, _)| *index));
        let maintenance = current_index.is_none();
        let index =
            current_index.unwrap_or_else(|| self.next_job_index.fetch_add(1, Ordering::SeqCst));
        let shared = self
            .current_job
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|(_, outcome)| Arc::clone(outcome)));
        let maintenance_job = maintenance.then(|| LineJob::maintenance(self, index));
        let outcome = self.writer().admit(CommitJob::stored(
            index,
            writes,
            held.keys.clone(),
            crate::chord::context::BACKGROUND_CONTEXT.clone(),
            Box::new(move |outcome| match outcome {
                JobOutcome::Stored(seq) => {
                    this.advance_deferred_metadata(&held.writes, &held.changes);
                    let result = this.publish_stored(
                        seq,
                        &held.writes,
                        &held.changes,
                        &crate::chord::context::BACKGROUND_CONTEXT,
                    );
                    this.release_pins(&held.pinned);
                    result
                }
                JobOutcome::Discard => {
                    this.release_pins(&held.pinned);
                    Ok(())
                }
                JobOutcome::Held => unreachable!(),
            }),
        ));
        if let Some(shared) = shared {
            let mut slot = shared.lock().await;
            *slot = Some(outcome);
            let result = slot
                .as_mut()
                .expect("deferred writer outcome")
                .await
                .map_err(|_| SessionError::Closed)?;
            *slot = None;
            result?;
        } else if let Some(mut job) = maintenance_job {
            job.track_outcome(outcome);
            let result = job.wait_writer().await;
            job.deliver(result).await?;
        } else {
            outcome.await.map_err(|_| SessionError::Closed)??;
        }
        Ok(())
    }

    /// Advance the cached incarnations of a maintenance-stored deferred
    /// commit: a stored base resets the delta count, a delta adds one
    /// (upstream `#storeHolds`'s publish callback).
    fn advance_deferred_metadata(
        &self,
        writes: &[crate::types::StorageWrite],
        changes: &[CommitChange],
    ) {
        let Ok(documents) = self.documents.lock() else {
            return;
        };
        for change in changes {
            let CommitChange::Document(document) = change else {
                continue;
            };
            let address = crate::documents::address_id(&crate::types::DocumentAddress {
                kind: document.record.kind.clone(),
                scope: document.record.scope.clone(),
                key: document.record.key.clone(),
            });
            let Some(loaded) = documents.get(&address) else {
                continue;
            };
            if loaded.record.id != document.record.id {
                continue;
            }
            let Some(crate::types::StorageWrite::DocumentChange { content, .. }) =
                writes.iter().find(|write| {
                    matches!(
                        write,
                        crate::types::StorageWrite::DocumentChange { id, .. }
                            if *id == document.record.id
                    )
                })
            else {
                continue;
            };
            if content.is_base() {
                loaded.set_deltas_since_base(0);
            } else {
                loaded.set_deltas_since_base(loaded.deltas_since_base() + 1);
            }
        }
    }

    fn release_pins(&self, pinned: &[DocumentId]) {
        if let Ok(mut set) = self.pinned.lock() {
            for id in pinned {
                set.remove(id);
            }
        }
    }

    fn publish(
        self: &Arc<Self>,
        seq: Seq,
        writes: &[crate::types::StorageWrite],
        documents: &[CommitChange],
        context: &Context,
    ) -> Result<(), PublishFailure> {
        self.publish_filtered(seq, writes, documents, context, None)
    }

    fn publish_taken(
        self: &Arc<Self>,
        seq: Seq,
        writes: &[crate::types::StorageWrite],
        observed: &[CommitChange],
        stored: &[CommitChange],
        context: &Context,
    ) -> Result<(), PublishFailure> {
        let internal = self.publish_filtered(seq, writes, observed, context, Some(true));
        let host = self.publish_filtered(seq, writes, stored, context, Some(false));
        match internal {
            Err(failure) => Err(failure),
            Ok(()) => host,
        }
    }

    fn publish_deferred(
        self: &Arc<Self>,
        documents: &[CommitChange],
        context: &Context,
    ) -> Result<(), PublishFailure> {
        self.publish_filtered(crate::ids::Seq(0), &[], documents, context, Some(true))
    }

    fn publish_stored(
        self: &Arc<Self>,
        seq: Seq,
        writes: &[crate::types::StorageWrite],
        documents: &[CommitChange],
        context: &Context,
    ) -> Result<(), PublishFailure> {
        self.publish_filtered(seq, writes, documents, context, Some(false))
    }

    fn publish_filtered(
        self: &Arc<Self>,
        seq: Seq,
        writes: &[crate::types::StorageWrite],
        documents: &[CommitChange],
        context: &Context,
        only_internal: Option<bool>,
    ) -> Result<(), PublishFailure> {
        let listeners: Vec<(CommitListener, bool)> = match self.commit_listeners.lock() {
            Ok(listeners) => listeners
                .iter()
                .map(|(_, listener, internal)| (Arc::clone(listener), *internal))
                .collect(),
            Err(_) => return Ok(()),
        };
        if listeners.is_empty() {
            return Ok(());
        }
        let mut changes = Vec::new();
        for write in writes {
            match write {
                crate::types::StorageWrite::Conversation { .. }
                | crate::types::StorageWrite::Entry { .. }
                | crate::types::StorageWrite::Task { .. }
                | crate::types::StorageWrite::Submission { .. } => {
                    changes.push(CommitChange::Table(write.clone()));
                }
                _ => {}
            }
        }
        changes.extend(documents.iter().cloned());
        let publication = CommitPublication { seq, changes };
        // Component listeners run before host listeners (upstream keeps two
        // sets and publishes to the internal one first).
        let mut ordered = listeners;
        ordered.sort_by_key(|(_, internal)| !*internal);
        let mut failure = None;
        for (listener, internal) in ordered {
            if only_internal.is_some_and(|expected| expected != internal) {
                continue;
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                listener(&publication, context);
            }));
            if let Err(payload) = outcome {
                let message = panic_message(payload);
                if internal {
                    if failure.is_none() {
                        failure = Some(PublishFailure::listener(SessionError::message(format!(
                            "A component commit listener failed: {message}"
                        ))));
                    }
                } else if let Ok(reporter) = self.listener_reporter.lock()
                    && let Some(reporter) = reporter.as_ref()
                {
                    reporter(
                        format!("A commit listener failed: {message}"),
                        Some(context.clone()),
                    );
                }
            }
        }
        match failure {
            Some(failure) => Err(failure),
            None => Ok(()),
        }
    }

    /// Run a read-only job on the mutation line (upstream `readOnLine`).
    pub fn read_on_line<T, F>(self: &Arc<Self>, job: F) -> SessionFuture<'_, T>
    where
        T: Send + 'static,
        F: FnOnce() -> SessionFuture<'static, T> + Send + 'static,
    {
        if let Err(error) = self.assert_usable() {
            return error_future(error);
        }
        let this = Arc::clone(self);
        Box::pin(async move {
            let _outstanding = this
                .outstanding_commits
                .acquire()
                .await
                .map_err(|_| SessionError::Closed)?;
            let guard = this.line.lock().await;
            let index = this.next_job_index.fetch_add(1, Ordering::SeqCst);
            let line_job = LineJob::new(&this, index);
            let result = std::panic::AssertUnwindSafe(ON_LINE.scope((), async {
                let _guard = guard;
                let _current = CurrentJobGuard::new(&this, index, &line_job.outcome);
                this.assert_healthy()?;
                job().await
            }))
            .catch_unwind()
            .await
            .unwrap_or_else(|payload| {
                Err(SessionError::message(format!(
                    "A read job panicked: {}",
                    panic_message(payload)
                )))
            });
            line_job.deliver(result).await
        })
    }

    // ─── Reads ──────────────────────────────────────────────────────────────

    /// Current committed value of one document, or `None` when absent (upstream `snapshot`).
    pub fn snapshot(
        self: &Arc<Self>,
        token: &DocToken,
        owner: DocOwner<'_>,
        context: &Context,
    ) -> SessionFuture<'_, Option<JsonObject>> {
        let definition = Arc::clone(&token.definition);
        let resolved = match resolve_address(&definition, doc_target(&definition, owner)) {
            Ok(resolved) => resolved,
            Err(error) => return error_future(error.into()),
        };
        if let Err(error) = self.assert_usable() {
            return error_future(error);
        }
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move {
            this.snapshot_resolved(&definition, resolved.id, resolved.address, &context)
                .await
        })
    }

    async fn snapshot_resolved(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: String,
        address: DocumentAddress,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        self.assert_usable()?;
        // A read is fenced behind earlier outcomes, including a cache hit:
        // the load still waits for the target key's write gate.
        if self.is_on_line() {
            return self
                .snapshot_load_on_line(definition, &address_id, &address, context)
                .await;
        }
        let this = Arc::clone(self);
        let definition = Arc::clone(definition);
        let context = context.clone();
        self.read_on_line(move || {
            Box::pin(async move {
                this.snapshot_load_on_line(&definition, &address_id, &address, &context)
                    .await
            })
        })
        .await
    }

    async fn snapshot_load_on_line(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: &str,
        address: &DocumentAddress,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        let loaded = self
            .load_document(definition, address_id, address, context)
            .await?;
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        check_record_scope_record(definition, &loaded.record)?;
        check_record_version_record(definition, &loaded.record, loaded.stored_version())?;
        Ok(loaded.tracker.value().as_object().cloned())
    }

    /// Attach a read-only replicated state to one document (upstream `documentState`).
    pub fn document_state(
        self: &Arc<Self>,
        token: &DocToken,
        owner: DocOwner<'_>,
        context: &Context,
    ) -> SessionFuture<'_, Option<crate::chord::state::AttachedReplicatedState>> {
        let definition = Arc::clone(&token.definition);
        let resolved = match resolve_address(&definition, doc_target(&definition, owner)) {
            Ok(resolved) => resolved,
            Err(error) => return error_future(error.into()),
        };
        if let Err(error) = self.assert_usable() {
            return error_future(error);
        }
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move {
            this.document_state_resolved(&definition, resolved.id, resolved.address, &context)
                .await
        })
    }

    async fn document_state_resolved(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: String,
        address: DocumentAddress,
        context: &Context,
    ) -> SessionResult<Option<crate::chord::state::AttachedReplicatedState>> {
        self.assert_usable()?;
        if self.is_on_line() {
            return self
                .document_state_on_line(definition, &address_id, &address, context)
                .await;
        }
        let this = Arc::clone(self);
        let definition = Arc::clone(definition);
        let context = context.clone();
        self.read_on_line(move || {
            Box::pin(async move {
                this.document_state_on_line(&definition, &address_id, &address, &context)
                    .await
            })
        })
        .await
    }

    async fn document_state_on_line(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: &str,
        address: &DocumentAddress,
        context: &Context,
    ) -> SessionResult<Option<crate::chord::state::AttachedReplicatedState>> {
        let loaded = self
            .load_document(definition, address_id, address, context)
            .await?;
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        check_record_scope_record(definition, &loaded.record)?;
        check_record_version_record(definition, &loaded.record, loaded.stored_version())?;
        let source = Arc::new(CommittedStateSource::new(loaded.tracker.value()));
        let observed = Arc::new(AtomicU64::new(loaded.value_version));
        let record_id = loaded.record.id;
        let source_commit = Arc::clone(&source);
        let subscription_commit =
            self.subscribe_internal_commits(Arc::new(move |publication, context| {
                for change in &publication.changes {
                    if let CommitChange::Document(document) = change {
                        if document.record.id != record_id {
                            continue;
                        }
                        let ops = observed_operations(&observed, document);
                        if ops.is_empty() {
                            continue;
                        }
                        source_commit.advance(
                            document.frame_value(),
                            ops,
                            without_abort_signal(context),
                        );
                    }
                }
            }));
        let source_close = Arc::clone(&source);
        let weak = Arc::downgrade(self);
        let subscription_close = self.subscribe_close(Arc::new(move || {
            let failure = weak.upgrade().and_then(|session| session.failure());
            source_close.close_session(failure);
        }));
        // Close/failure may have won while the document loaded. Upstream
        // `subscribeClose` throws from `assertUsable`; the Rust registration is
        // inert then, so reject the attachment instead of returning one that
        // can never end.
        if let Err(error) = self.assert_usable() {
            subscription_commit.unsubscribe();
            subscription_close.unsubscribe();
            return Err(error);
        }
        self.attach_document(address_id, record_id);
        let weak = Arc::downgrade(self);
        let attached = address_id.to_string();
        source.set_release(Box::new(move || {
            subscription_commit.unsubscribe();
            subscription_close.unsubscribe();
            if let Some(session) = weak.upgrade() {
                session.detach_document(&attached, record_id);
            }
        }));
        Ok(Some(attach_state(&source)))
    }

    /// Attach a synchronous observer atomically to one document address.
    pub fn observe_doc<'a>(
        self: &Arc<Self>,
        token: &DocToken,
        owner: DocOwner<'a>,
        observer: Arc<dyn crate::session::Observer<crate::session::ObservedDocumentValue>>,
        context: &Context,
    ) -> SessionFuture<'a, Option<crate::session::Observation<crate::session::ObservedDocumentValue>>>
    {
        let definition = Arc::clone(&token.definition);
        let resolved = match resolve_address(&definition, doc_target(&definition, owner)) {
            Ok(resolved) => resolved,
            Err(error) => return error_future(error.into()),
        };
        if let Err(error) = self.assert_usable() {
            return error_future(error);
        }
        let signal = context.abort_signal();
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move {
            let owner_task = match owner {
                DocOwner::Task(task) | DocOwner::TaskFamily(task, _) => Some(task),
                _ => None,
            };
            let Some(watch) = this
                .watch_doc_resolved(
                    &definition,
                    resolved.id,
                    resolved.address,
                    &context,
                    signal,
                    owner_task,
                )
                .await?
            else {
                return Ok(None);
            };
            let weak = Arc::downgrade(&this);
            let reporter = Arc::new(move |message: String, context: Option<Context>| {
                if let Some(session) = weak.upgrade()
                    && let Ok(slot) = session.listener_reporter.lock()
                    && let Some(reporter) = slot.as_ref()
                {
                    reporter(message, context);
                }
            });
            Ok(Some(watch.observe(observer, reporter)))
        })
    }

    /// Attach a serialized async watch to one document (upstream `watchDoc`).
    pub fn watch_doc<'a>(
        self: &Arc<Self>,
        token: &DocToken,
        owner: DocOwner<'a>,
        context: &Context,
    ) -> SessionFuture<'a, Option<CommittedWatch>> {
        let definition = Arc::clone(&token.definition);
        let resolved = match resolve_address(&definition, doc_target(&definition, owner)) {
            Ok(resolved) => resolved,
            Err(error) => return error_future(error.into()),
        };
        if let Err(error) = self.assert_usable() {
            return error_future(error);
        }
        let signal = context.abort_signal();
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move {
            let owner_task = match owner {
                DocOwner::Task(task) | DocOwner::TaskFamily(task, _) => Some(task),
                _ => None,
            };
            this.watch_doc_resolved(
                &definition,
                resolved.id,
                resolved.address,
                &context,
                signal,
                owner_task,
            )
            .await
        })
    }

    async fn watch_doc_resolved(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: String,
        address: DocumentAddress,
        context: &Context,
        signal: Option<AbortSignal>,
        owner_task: Option<crate::ids::TaskId>,
    ) -> SessionResult<Option<CommittedWatch>> {
        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return Err(SessionError::Aborted);
        }
        if self.is_on_line() {
            return self
                .watch_doc_on_line(
                    definition,
                    &address_id,
                    &address,
                    context,
                    signal,
                    owner_task,
                )
                .await;
        }
        let this = Arc::clone(self);
        let definition = Arc::clone(definition);
        let context = context.clone();
        self.read_on_line(move || {
            Box::pin(async move {
                this.watch_doc_on_line(
                    &definition,
                    &address_id,
                    &address,
                    &context,
                    signal,
                    owner_task,
                )
                .await
            })
        })
        .await
    }

    async fn watch_doc_on_line(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: &str,
        address: &DocumentAddress,
        context: &Context,
        signal: Option<AbortSignal>,
        owner_task: Option<crate::ids::TaskId>,
    ) -> SessionResult<Option<CommittedWatch>> {
        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return Err(SessionError::Aborted);
        }
        let loaded = self
            .load_document(definition, address_id, address, context)
            .await?;
        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return Err(SessionError::Aborted);
        }
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        check_record_scope_record(definition, &loaded.record)?;
        check_record_version_record(definition, &loaded.record, loaded.stored_version())?;
        let watch = CommittedWatch::new(loaded.tracker.value());
        let observed = Arc::new(AtomicU64::new(loaded.value_version));
        let observed_id = Arc::new(AtomicU64::new(loaded.record.id.get()));
        let record_id = loaded.record.id;
        let watch_commit = watch.clone();
        let watched_address = address_id.to_string();
        let watch_owner = watch.clone();
        let watch_session = Arc::downgrade(self);
        let detach_id = Arc::clone(&observed_id);
        let subscription_commit =
            self.subscribe_internal_commits(Arc::new(move |publication, context| {
                for change in &publication.changes {
                    match change {
                        CommitChange::Document(document) => {
                            let changed_address =
                                crate::documents::address_id(&crate::types::DocumentAddress {
                                    kind: document.record.kind.clone(),
                                    scope: document.record.scope.clone(),
                                    key: document.record.key.clone(),
                                });
                            if changed_address != watched_address {
                                continue;
                            }
                            let retired = document.is_retirement();
                            if retired {
                                if document.record.id.get() != observed_id.load(Ordering::SeqCst) {
                                    continue;
                                }
                                if let Some(session) = watch_session.upgrade() {
                                    session.detach_document(&watched_address, document.record.id);
                                }
                                watch_commit.advance(
                                    document.frame_value(),
                                    Vec::new(),
                                    true,
                                    context.clone(),
                                );
                                continue;
                            }
                            let previous_id = observed_id.load(Ordering::SeqCst);
                            let ops = observed_operations_for(&observed_id, &observed, document);
                            if document.record.id.get() != previous_id
                                && let Some(session) = watch_session.upgrade()
                            {
                                session.detach_document(
                                    &watched_address,
                                    DocumentId::from_number(previous_id),
                                );
                                session.attach_document(&watched_address, document.record.id);
                            }
                            watch_commit.advance(
                                document.frame_value(),
                                ops,
                                false,
                                context.clone(),
                            );
                        }
                        CommitChange::Table(crate::types::StorageWrite::Task { value })
                            if owner_task == Some(value.id)
                                && matches!(
                                    value.state,
                                    crate::types::TaskState::Terminal { .. }
                                ) =>
                        {
                            watch_owner.close_session(None);
                        }
                        _ => {}
                    }
                }
            }));
        let watch_close = watch.clone();
        let weak = Arc::downgrade(self);
        let subscription_close = self.subscribe_close(Arc::new(move || {
            let failure = weak.upgrade().and_then(|session| session.failure());
            watch_close.close_session(failure);
        }));
        // As `document_state_resolved`: a registration during close is inert,
        // so a watch created here would never receive `end`.
        if let Err(error) = self.assert_usable() {
            subscription_commit.unsubscribe();
            subscription_close.unsubscribe();
            return Err(error);
        }
        self.attach_document(address_id, record_id);
        let weak = Arc::downgrade(self);
        let attached = address_id.to_string();
        watch.set_detach(Box::new(move || {
            subscription_commit.unsubscribe();
            subscription_close.unsubscribe();
            if let Some(session) = weak.upgrade() {
                session.detach_document(
                    &attached,
                    DocumentId::from_number(detach_id.load(Ordering::SeqCst)),
                );
            }
        }));
        if let Some(signal) = signal {
            if signal.is_cancelled() {
                watch.cancel();
                return Err(SessionError::Aborted);
            }
            let watch_cancel = watch.clone();
            tokio::spawn(async move {
                signal.cancelled().await;
                watch_cancel.cancel();
            });
        }
        Ok(Some(watch))
    }

    /// Historical read of one conversation document (upstream `snapshotAsOf`).
    pub fn snapshot_as_of(
        self: &Arc<Self>,
        token: &DocToken,
        conversation: ConversationId,
        at: EntryId,
        context: &Context,
    ) -> SessionFuture<'_, Option<JsonObject>> {
        let definition = Arc::clone(&token.definition);
        if !matches!(definition.scope, DocScope::Conversation) {
            return error_future(SessionError::message(
                "Session.snapshot_as_of() requires a conversation document",
            ));
        }
        if let Err(error) = self.assert_usable() {
            return error_future(error);
        }
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move {
            this.snapshot_as_of_resolved(&definition, conversation, at, &context)
                .await
        })
    }

    async fn snapshot_as_of_resolved(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        conversation: ConversationId,
        at: EntryId,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        self.assert_usable()?;
        if self.is_on_line() {
            return self
                .snapshot_as_of_on_line(definition, conversation, at, context)
                .await;
        }
        let this = Arc::clone(self);
        let definition = Arc::clone(definition);
        let context = context.clone();
        self.read_on_line(move || {
            Box::pin(async move {
                this.snapshot_as_of_on_line(&definition, conversation, at, &context)
                    .await
            })
        })
        .await
    }

    async fn snapshot_as_of_on_line(
        self: &Arc<Self>,
        definition: &Arc<crate::documents::DocDefinition>,
        conversation: ConversationId,
        at: EntryId,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        let stored_entry = self
            .storage()
            .entry_in_conversation(conversation, at, context)
            .await?
            .ok_or_else(|| {
                SessionError::message(format!(
                    "Entry {at} is not visible from conversation {conversation}"
                ))
            })?;
        let address = DocumentAddress {
            kind: definition.kind.clone(),
            scope: DocumentScope::Conversation {
                conversation_id: stored_entry.entry.conversation_id,
            },
            key: None,
        };
        let point = crate::types::DocumentPoint::At(stored_entry.commit_seq);
        let record = self
            .storage()
            .find_document(address, point, context)
            .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        let stored = self
            .storage()
            .document(record.id, point, context)
            .await?
            .ok_or_else(|| {
                SessionError::message(format!(
                    "Historical document {} ({}) cannot be read",
                    record.id, record.kind
                ))
            })?;
        Ok(Some(materialize_document(definition, &stored)?))
    }

    /// Current conversation document resolved on the line (upstream
    /// `conversationDocumentOnLine`).
    pub async fn conversation_document_on_line(
        self: &Arc<Self>,
        token: &DocToken,
        conversation_id: ConversationId,
        context: &Context,
    ) -> SessionResult<Option<ConversationDocumentView>> {
        let definition = Arc::clone(&token.definition);
        let target = DocTarget::Conversation {
            conversation_id,
            key: None,
        };
        let resolved = resolve_address(&definition, target)?;
        let loaded = self
            .load_document(&definition, &resolved.id, &resolved.address, context)
            .await?;
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        check_record_scope_record(&definition, &loaded.record)?;
        check_record_version_record(&definition, &loaded.record, loaded.stored_version())?;
        Ok(Some(ConversationDocumentView {
            record: loaded.record.clone(),
            version: loaded.value_version,
            value: loaded
                .tracker
                .value()
                .as_object()
                .cloned()
                .unwrap_or_default(),
        }))
    }

    // ─── Cache ──────────────────────────────────────────────────────────────

    /// Drop every loaded tracker on the line (upstream `unloadDocuments`).
    pub fn unload_documents(self: &Arc<Self>) -> SessionFuture<'_, ()> {
        let this = Arc::clone(self);
        Box::pin(async move {
            let _guard = this.line.lock().await;
            this.flush_deferred_on_line().await?;
            if let Ok(mut documents) = this.documents.lock() {
                documents.clear();
            }
            if let Ok(mut idle) = this.idle_since.lock() {
                idle.clear();
            }
            Ok(())
        })
    }

    // ─── Close ──────────────────────────────────────────────────────────────

    /// Seal admission and close storage (upstream `close`).
    pub fn close(self: &Arc<Self>, context: &Context) -> SessionFuture<'_, ()> {
        if !self.closing.swap(true, Ordering::SeqCst) {
            self.notify_close();
        }
        let this = Arc::clone(self);
        let context = without_abort_signal(context);
        Box::pin(async move {
            let _guard = this.line.lock().await;
            if this.cleanup_done.swap(true, Ordering::SeqCst) {
                this.settle_end();
                if let Some(error) = this.close_error.lock().ok().and_then(|error| error.clone()) {
                    return Err(error.into_session_error());
                }
                return Ok(());
            }
            let mut reset = ResetOnDrop::new(&this.cleanup_done);
            this.writer().drained().await;
            if let Ok(mut pending) = this.document_sweep.lock()
                && let Some(sweep) = pending.take()
                && let Some(disarm) = sweep.disarm
            {
                disarm();
            }
            let deferred_result = if this.failure().is_none() {
                this.flush_deferred_on_line().await
            } else {
                Ok(())
            };
            if let Ok(mut deferred) = this.deferred.lock() {
                deferred.take();
            }
            this.deferred_pending.store(false, Ordering::SeqCst);
            this.end_commit_observers();
            if let Ok(mut documents) = this.documents.lock() {
                documents.clear();
            }
            if let Ok(mut idle) = this.idle_since.lock() {
                idle.clear();
            }
            if let Ok(mut pinned) = this.pinned.lock() {
                pinned.clear();
            }
            if let Ok(mut attached) = this.attached.lock() {
                attached.clear();
            }
            if let Ok(mut listeners) = this.commit_listeners.lock() {
                listeners.clear();
            }
            this.writer().shutdown();
            let result = this.guarded.close(&context).await;
            if let Err(error) = &result {
                this.fail(error.to_string());
            }
            let outcome = match result {
                Ok(()) => deferred_result,
                Err(error) => Err(error.into()),
            };
            if let Err(error) = &outcome
                && let Ok(mut close_error) = this.close_error.lock()
            {
                *close_error = Some(CloseError::from_session_error(error));
            }
            // `closed` settles only after the backend close, like upstream.
            this.settle_end();
            reset.disarm();
            outcome
        })
    }

    /// Resolve the terminal session state once closing has finished.
    fn settle_end(&self) {
        match self.failure() {
            Some(error) => self.set_end(SessionEnd::Failed { error }),
            None => self.set_end(SessionEnd::Closed),
        }
    }

    // ─── Listeners ──────────────────────────────────────────────────────────

    /// Register a synchronous post-adoption host commit listener. This is the
    /// Rust lower-level counterpart of the `observeCommits()` dispatch; upstream
    /// has no separate `subscribeCommits` method and schedules host observers as
    /// a microtask (D-111 registers the synchronous Rust form).
    pub fn subscribe_commits(&self, listener: CommitListener) -> Subscription {
        if self.assert_usable().is_err() {
            return Subscription::new(|| {});
        }
        let store = Arc::clone(&self.commit_listeners);
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut listeners) = store.lock() {
            listeners.push((id, listener, false));
        }
        let subscription = Subscription::new(move || {
            if let Ok(mut listeners) = store.lock() {
                listeners.retain(|(existing, _, _)| *existing != id);
            }
        });
        if self.assert_usable().is_err() {
            subscription.unsubscribe();
            return Subscription::new(|| {});
        }
        subscription
    }

    /// Observe stored publications without replaying a synthetic value.
    pub fn observe_commits(
        self: &Arc<Self>,
        observer: Arc<dyn crate::session::CommitObserver>,
    ) -> crate::session::CommitObservation {
        // Upstream `observeCommits()` calls `assertUsable()` and throws. The
        // Rust API has no error return here, so an observation registered after
        // close is ended immediately instead of being stored where it would
        // never receive `end` (the close path already drained the observers).
        if let Err(error) = self.assert_usable() {
            let end = match error {
                SessionError::Failed { cause } => crate::session::WatchEnd::Failed { error: cause },
                _ => crate::session::WatchEnd::Ended,
            };
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer.end(end))).is_err()
            {
                self.report_listener_error("commit observer end panicked".to_string(), None);
            }
            return crate::session::CommitObservation {
                detach: Subscription::new(|| {}),
            };
        }
        let active = Arc::new(AtomicBool::new(true));
        let observer_id = NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut observers) = self.commit_observers.lock() {
            observers.push((observer_id, Arc::clone(&observer), Arc::clone(&active)));
        }
        let detachment = Arc::new(Mutex::new(None::<crate::session::Subscription>));
        let commit_subscription = {
            let active = Arc::clone(&active);
            let detachment = Arc::clone(&detachment);
            let observer = Arc::clone(&observer);
            let weak = Arc::downgrade(self);
            self.subscribe_commits(Arc::new(move |publication, context| {
                if !active.load(Ordering::SeqCst) {
                    return;
                }
                let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    observer.advance(publication, context);
                }));
                if failed.is_err() && active.swap(false, Ordering::SeqCst) {
                    observer.end(crate::session::WatchEnd::ListenerError {
                        error: "commit observer panicked".to_string(),
                    });
                    if let Some(session) = weak.upgrade()
                        && let Ok(reporter) = session.listener_reporter.lock()
                        && let Some(reporter) = reporter.as_ref()
                    {
                        reporter(
                            "listener_error: commit observer panicked".to_string(),
                            Some(context.clone()),
                        );
                    }
                    if let Ok(mut slot) = detachment.lock()
                        && let Some(subscription) = slot.take()
                    {
                        subscription.unsubscribe();
                    }
                }
            }))
        };
        let observers = Arc::clone(&self.commit_observers);
        let detach_active = Arc::clone(&active);
        let detach = Subscription::new(move || {
            detach_active.store(false, Ordering::SeqCst);
            commit_subscription.unsubscribe();
            if let Ok(mut registered) = observers.lock() {
                registered.retain(|(id, _, _)| *id != observer_id);
            }
        });
        let detach = Arc::new(detach);
        if let Ok(mut slot) = detachment.lock() {
            *slot = Some(Subscription::new({
                let detach = Arc::clone(&detach);
                move || detach.unsubscribe()
            }));
        }
        if let Err(error) = self.assert_usable() {
            let end = match error {
                SessionError::Failed { cause } => crate::session::WatchEnd::Failed { error: cause },
                _ => crate::session::WatchEnd::Ended,
            };
            let should_end = active.swap(false, Ordering::SeqCst);
            detach.unsubscribe();
            if should_end
                && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer.end(end)))
                    .is_err()
            {
                self.report_listener_error("commit observer end panicked".to_string(), None);
            }
        }
        crate::session::CommitObservation {
            detach: Subscription::new(move || detach.unsubscribe()),
        }
    }

    fn end_commit_observers(&self) {
        let observers = match self.commit_observers.lock() {
            Ok(mut registered) => registered.drain(..).collect::<Vec<_>>(),
            Err(_) => return,
        };
        let end = match self.failure() {
            Some(error) => crate::session::WatchEnd::Failed { error },
            None => crate::session::WatchEnd::Ended,
        };
        for (_, observer, active) in observers {
            if !active.swap(false, Ordering::SeqCst) {
                continue;
            }
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.end(end.clone());
            }))
            .is_err()
            {
                self.report_listener_error("commit observer end panicked".to_string(), None);
            }
        }
    }

    /// Register the Session's own component commit listener: its throw fails
    /// the Session (upstream `subscribeInternalCommits`).
    pub fn subscribe_internal_commits(&self, listener: CommitListener) -> Subscription {
        if self.assert_usable().is_err() {
            return Subscription::new(|| {});
        }
        let store = Arc::clone(&self.commit_listeners);
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut listeners) = store.lock() {
            listeners.push((id, listener, true));
        }
        let subscription = Subscription::new(move || {
            if let Ok(mut listeners) = store.lock() {
                listeners.retain(|(existing, _, _)| *existing != id);
            }
        });
        if self.assert_usable().is_err() {
            subscription.unsubscribe();
            return Subscription::new(|| {});
        }
        subscription
    }

    /// Register a synchronous close listener (upstream `subscribeClose`).
    pub fn subscribe_close(&self, listener: CloseListener) -> Subscription {
        if self.assert_usable().is_err() {
            return Subscription::new(|| {});
        }
        let store = Arc::clone(&self.close_listeners);
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut listeners) = store.lock() {
            listeners.push((id, listener));
        }
        let subscription_store = Arc::clone(&store);
        let subscription = Subscription::new(move || {
            if let Ok(mut listeners) = subscription_store.lock() {
                listeners.retain(|(existing, _)| *existing != id);
            }
        });
        if self.assert_usable().is_err() {
            // Close/failure won between the check and the push: a listener
            // still in the set never saw `notify_close`, so call it now (once,
            // as the registration is immediately closed).
            let removed = store.lock().ok().and_then(|mut listeners| {
                listeners
                    .iter()
                    .position(|(existing, _)| *existing == id)
                    .map(|index| listeners.remove(index).1)
            });
            subscription.unsubscribe();
            if let Some(listener) = removed
                && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener())).is_err()
            {
                self.report_listener_error("A close listener panicked".to_string(), None);
            }
            return Subscription::new(|| {});
        }
        subscription
    }

    fn assert_usable(&self) -> SessionResult<()> {
        self.assert_healthy()?;
        if self.closing.load(Ordering::SeqCst) {
            return Err(SessionError::Closed);
        }
        Ok(())
    }

    fn assert_healthy(&self) -> SessionResult<()> {
        if let Some(cause) = self.failure() {
            return Err(SessionError::Failed { cause });
        }
        Ok(())
    }
}

fn split_deferred(
    held: DeferredCommit,
    only: Option<DocumentId>,
) -> (Option<DeferredCommit>, Option<DeferredCommit>) {
    let Some(only) = only else {
        return (Some(held), None);
    };
    if !held.writes.iter().any(|write| {
        matches!(write, crate::types::StorageWrite::DocumentChange { id, .. } if *id == only)
    }) {
        return (None, Some(held));
    }
    let DeferredCommit {
        id,
        writes,
        changes,
        pinned,
        deadlines,
        replaces,
        ..
    } = held;
    let mut selected_writes = Vec::new();
    let mut remaining_writes = Vec::new();
    for write in writes {
        if matches!(&write, crate::types::StorageWrite::DocumentChange { id, .. } if *id == only) {
            selected_writes.push(write);
        } else {
            remaining_writes.push(write);
        }
    }
    let mut selected_changes = Vec::new();
    let mut remaining_changes = Vec::new();
    for change in changes {
        if matches!(&change, CommitChange::Document(document) if document.record.id == only) {
            selected_changes.push(change);
        } else {
            remaining_changes.push(change);
        }
    }
    let selected_pinned = pinned
        .iter()
        .copied()
        .filter(|id| *id == only)
        .collect::<Vec<_>>();
    let remaining_pinned = pinned
        .into_iter()
        .filter(|id| *id != only)
        .collect::<Vec<_>>();
    let selected_deadlines = deadlines
        .iter()
        .filter(|(id, _)| **id == only)
        .map(|(id, deadline)| (*id, *deadline))
        .collect::<HashMap<_, _>>();
    let remaining_deadlines = deadlines
        .into_iter()
        .filter(|(id, _)| *id != only)
        .collect::<HashMap<_, _>>();
    let selected_replaces = replaces
        .iter()
        .copied()
        .filter(|id| *id == only)
        .collect::<std::collections::HashSet<_>>();
    let remaining_replaces = replaces
        .into_iter()
        .filter(|id| *id != only)
        .collect::<std::collections::HashSet<_>>();
    let selected = DeferredCommit {
        id,
        deadline: crate::session::earliest_deferred_deadline(&selected_deadlines),
        deadlines: selected_deadlines,
        writes: selected_writes,
        keys: crate::session::transaction::deferred_keys(&selected_changes),
        changes: selected_changes,
        pinned: selected_pinned,
        replaces: selected_replaces,
    };
    let has_remaining = !remaining_writes.is_empty();
    let remaining = DeferredCommit {
        id,
        deadline: crate::session::earliest_deferred_deadline(&remaining_deadlines),
        deadlines: remaining_deadlines,
        writes: remaining_writes,
        keys: crate::session::transaction::deferred_keys(&remaining_changes),
        changes: remaining_changes,
        pinned: remaining_pinned,
        replaces: remaining_replaces,
    };
    (Some(selected), has_remaining.then_some(remaining))
}

fn merge_deferred_writes(
    previous: Option<&Vec<crate::types::StorageWrite>>,
    current: Vec<crate::types::StorageWrite>,
) -> Vec<crate::types::StorageWrite> {
    let mut merged = previous.cloned().unwrap_or_default();
    for write in current {
        let crate::types::StorageWrite::DocumentChange { id, content } = &write else {
            merged.push(write);
            continue;
        };
        let existing = merged.iter_mut().find(|existing| {
            matches!(existing, crate::types::StorageWrite::DocumentChange { id: existing_id, .. } if existing_id == id)
        });
        if let Some(crate::types::StorageWrite::DocumentChange {
            content: existing_content,
            ..
        }) = existing
        {
            match (existing_content, content) {
                (
                    crate::types::DocumentContent::Delta {
                        version: existing_version,
                        ops: existing_ops,
                    },
                    crate::types::DocumentContent::Delta { version, ops },
                ) if existing_version == version => existing_ops.extend(ops.iter().cloned()),
                (
                    crate::types::DocumentContent::Base { .. },
                    crate::types::DocumentContent::Delta { .. },
                ) => {}
                (existing_content, content) => {
                    *existing_content = content.clone();
                }
            }
        } else {
            merged.push(write);
        }
    }
    merged
}

fn merge_deferred_changes(
    previous: Option<Vec<CommitChange>>,
    current: Vec<CommitChange>,
    replaces: &std::collections::HashSet<crate::ids::DocumentId>,
) -> Vec<CommitChange> {
    let mut merged = previous.unwrap_or_default();
    for change in current {
        let CommitChange::Document(document) = &change else {
            merged.push(change);
            continue;
        };
        let existing = merged.iter_mut().find(|existing| {
            matches!(existing, CommitChange::Document(existing_document) if existing_document.record.id == document.record.id)
        });
        if let Some(CommitChange::Document(existing_document)) = existing {
            if replaces.contains(&document.record.id) {
                // The held pending operations are a replacement: later
                // operations are covered by the current value (upstream
                // `PendingOps` ignores pushes in its `replace` state).
                *existing_document = document.clone();
                continue;
            }
            let mut operations = std::mem::take(&mut existing_document.ops);
            operations.extend(document.ops.iter().cloned());
            *existing_document = document.clone();
            existing_document.ops = operations;
        } else {
            merged.push(change);
        }
    }
    merged
}

fn compact_deferred_operations(
    writes: &mut [crate::types::StorageWrite],
    changes: &mut [CommitChange],
    replaces: &mut std::collections::HashSet<crate::ids::DocumentId>,
) {
    let mut operations_by_document = std::collections::HashMap::new();
    for change in &mut *changes {
        let CommitChange::Document(document) = change else {
            continue;
        };
        let id = document.record.id;
        if replaces.contains(&id) {
            // Upstream `Hold.pending` stays in its `replace` state: later
            // operations are covered by the current value, and the stored
            // change is the replacement.
            let operations = vec![crate::chord::delta::Op::Replace(document.frame_value())];
            operations_by_document.insert(id, operations.clone());
            document.ops = operations;
            continue;
        }
        let mut pending = crate::chord::delta::PendingOps::new();
        pending.push(&document.ops);
        let operations = match pending.take() {
            crate::chord::delta::PendingTake::Ops(operations) => operations,
            crate::chord::delta::PendingTake::Replace => {
                replaces.insert(id);
                vec![crate::chord::delta::Op::Replace(document.frame_value())]
            }
        };
        operations_by_document.insert(id, operations.clone());
        document.ops = operations;
    }
    for write in writes {
        let crate::types::StorageWrite::DocumentChange { id, content } = write else {
            continue;
        };
        if replaces.contains(id) {
            if let Some(document) = changes.iter().find_map(|change| match change {
                CommitChange::Document(document) if document.record.id == *id => Some(document),
                _ => None,
            }) {
                // An overflowed held commit stores the whole value as a base,
                // as upstream `storeHeld` does for `replace`.
                *content = crate::types::DocumentContent::Base {
                    version: document.version.unwrap_or(0),
                    value: document.value.clone().unwrap_or_default(),
                };
            }
            continue;
        }
        match content {
            crate::types::DocumentContent::Delta { ops, .. } => {
                if let Some(operations) = operations_by_document.get(id) {
                    *ops = operations.clone();
                }
            }
            crate::types::DocumentContent::Base { value, version } => {
                if let Some(document) = changes.iter().find_map(|change| match change {
                    CommitChange::Document(document) if document.record.id == *id => Some(document),
                    _ => None,
                }) {
                    *value = document.value.clone().unwrap_or_default();
                    *version = document.version.unwrap_or(*version);
                }
            }
        }
    }
}

/// The message of a caught panic payload.
pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Install a watch listener helper used by tests and consumers.
pub fn watch_listener<F>(function: F) -> WatchListener
where
    F: Fn(crate::session::WatchCall, Context) -> crate::session::WatchFuture
        + Send
        + Sync
        + 'static,
{
    Arc::new(function)
}
