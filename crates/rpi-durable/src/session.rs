//! Session kernel of the durable runtime (port of the session domain of
//! `packages/durable/src/session/*` @ `b4afdddf`: `session.ts`, `transaction.ts`,
//! `observation.ts`, `forks.ts`).
//!
//! The kernel owns one **mutation line**: commit callbacks and settlement are
//! serialized, then the Writer stores admitted batches off the line and
//! publishes them in admission order; only committed state is observable. This module root carries the shared error
//! surface and the public value types; [`session`] carries the single-line
//! [`session::SessionImpl`], [`transaction`] the staging [`transaction::Transaction`],
//! [`observation`] the async watch/state bridge, and [`forks`] the conversation
//! document copy planner.
//!
//! Upstream is a JavaScript implementation built on promises and microtasks; the
//! Rust port maps the line to a `tokio::sync::Mutex`, callbacks to boxed futures,
//! and `Draft<T>` proxies to [`crate::chord::tracker::DocCell`] (registered shape
//! differences, V17-14 §7.3).

use std::future::Future;
use std::pin::Pin;

use thiserror::Error;

use crate::chord::delta::{DeltaError, Op};
use crate::chord::json::{JsonObject, JsonValue};
use crate::documents::DocumentError;
use crate::errors::StorageError;
use crate::ids::{ConversationId, Seq, TaskId};
use crate::types::{DocumentCopySource, DocumentRecord, StorageWrite};

pub mod cached_storage;
pub mod forks;
mod gated;
pub mod guarded;
mod keys;
pub mod observation;
// The design keeps the kernel in `session/session.rs` (frozen layout); Clippy's
// `module_inception` is a style lint for the resulting `session::session` path.
#[allow(clippy::module_inception)]
pub mod session;
pub mod transaction;
mod writer;

pub use cached_storage::{CachedStorage, CachedStorageSizes};
pub use forks::{ForkDocumentCopy, prepare_fork_document_copies};
pub use observation::{
    CommittedStateSource, CommittedWatch, ObservedDocumentValue, RETIREMENT_OPERATIONS, WatchCall,
    WatchEnd, WatchFuture, WatchListener,
};
pub use session::{
    CloseListener, CommitFn, CommitListener, ConversationCreatedHook, ConversationDocumentView,
    NowFn, SessionImpl, commit_fn, create_session, watch_listener,
};
pub(crate) use transaction::doc_target;
pub use transaction::{
    ConversationOwnership, DocDraft, EntryDraft, EntryHead, LoadedDocument, SubmissionCreate,
    SubmissionSettlement, TaskCreate, TaskOwnership, TaskTrace, Transaction, TransactionScope,
};

/// An idempotent unsubscribe handle (upstream's returned unsubscribe function).
pub struct Subscription {
    unsubscribe: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Subscription {
    pub(crate) fn new(unsubscribe: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            unsubscribe: std::sync::Mutex::new(Some(Box::new(unsubscribe))),
        }
    }

    /// Stop future callbacks. Idempotent.
    pub fn unsubscribe(&self) {
        if let Ok(mut slot) = self.unsubscribe.lock()
            && let Some(unsubscribe) = slot.take()
        {
            unsubscribe();
        }
    }
}

/// A boxed session future; every public session operation that is not a bare
/// accessor returns one of these so callers can `.await` uniformly.
pub type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SessionError>> + Send + 'a>>;

/// Failure surface of the session kernel.
///
/// Upstream throws plain `Error` subclasses; the Rust port keeps the original
/// storage error reachable through [`SessionError::Storage`], except for the
/// guard's own failure marker, which becomes [`SessionError::Failed`].
#[derive(Debug, Error)]
pub enum SessionError {
    /// Admission is sealed; every later operation rejects (upstream
    /// `"Session is closed"`).
    #[error("Session is closed")]
    Closed,
    /// A read-only Session rejected a write before reaching Storage.
    #[error("Read-only session")]
    ReadOnlySession,
    /// The Session failed after a storage error; nothing runs or commits any
    /// more. Reopen the Session to recover from what was committed.
    #[error("Session failed after a storage error; close and reopen it")]
    Failed {
        /// The first error's message.
        cause: String,
    },
    /// Storage failure surfaced unchanged, including `StorageRequestError` and
    /// `ReadAfterWrite`.
    #[error(transparent)]
    Storage(StorageError),
    /// Typed document definition mismatch (`documents.ts` errors).
    #[error(transparent)]
    Document(#[from] DocumentError),
    /// Delta application failure.
    #[error(transparent)]
    Delta(#[from] DeltaError),
    /// Caller cancellation observed before admission.
    #[error("The operation was aborted")]
    Aborted,
    /// Transaction, validation, or callback failure carrying the upstream text.
    #[error("{0}")]
    Message(String),
}

impl From<StorageError> for SessionError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::SessionFailed(cause) => Self::Failed { cause },
            error => Self::Storage(error),
        }
    }
}

impl SessionError {
    /// Build a message error with the upstream text.
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }

    /// Build a callback failure error with the caller's message.
    pub fn callback(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }
}

/// Result alias for session operations.
pub type SessionResult<T> = Result<T, SessionError>;

/// How a Session ended (upstream `SessionEnd`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// Closed by its host.
    Closed,
    /// Failed after a storage error; carries the first error's message.
    Failed {
        /// First error's message.
        error: String,
    },
}

/// Owner argument of a typed document access (upstream `doc`/`snapshot` overload
/// arguments).
///
/// The frozen design listed `{ Session, Conversation, Task, Family }`; the Rust
/// port adds explicit family-member variants because a conversation/task-scoped
/// family member needs both its scope owner and its key (shape difference: the
/// frozen enum could not express those overloads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocOwner<'a> {
    /// Session-scoped singleton.
    Session,
    /// Session-scoped family member.
    Family(&'a str),
    /// Conversation-scoped singleton.
    Conversation(ConversationId),
    /// Conversation-scoped family member.
    ConversationFamily(ConversationId, &'a str),
    /// Task-scoped singleton.
    Task(TaskId),
    /// Task-scoped family member.
    TaskFamily(TaskId, &'a str),
}

/// One table or document change carried by a [`CommitPublication`].
#[derive(Debug, Clone, PartialEq)]
// The upstream union carries the full record on its table arm.
#[allow(clippy::large_enum_variant)]
pub enum CommitChange {
    /// A table write (`conversation`/`entry`/`task`/`submission`), in staging order.
    Table(StorageWrite),
    /// A document content change (create/change) or incarnation retirement.
    Document(DocumentChange),
    /// A definition-free conversation document copy.
    DocumentCopy(DocumentCopyChange),
}

/// A committed document value change or incarnation retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentChangeKind {
    Change,
    Retire,
}

/// One committed document content change or retirement (upstream
/// `DocumentCommitChange` with `type: "document"`).
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentChange {
    /// Whether this is a value change or an incarnation retirement.
    pub kind: DocumentChangeKind,
    /// Incarnation record after the commit; `retired_at` is stamped on retirement.
    pub record: DocumentRecord,
    /// Publication conversation; absent for session-scoped documents.
    pub conversation_id: Option<ConversationId>,
    /// Definition version of `value`; absent for a retirement.
    pub version: Option<u64>,
    /// Committed value; absent for a retirement.
    pub value: Option<JsonObject>,
    /// Exact operation batch that produced `value`; empty for a creation or a
    /// migration-only base.
    pub ops: Vec<Op>,
}

impl DocumentChange {
    /// Whether this frame retires the incarnation.
    pub fn is_retirement(&self) -> bool {
        self.kind == DocumentChangeKind::Retire
    }

    /// The frame value as strict JSON (`null` for a retirement).
    pub fn frame_value(&self) -> JsonValue {
        match &self.value {
            None => JsonValue::Null,
            Some(value) => JsonValue::Object(value.clone()),
        }
    }
}

/// One committed conversation document copy (upstream
/// `DocumentCommitChange` with `type: "document.copy"`).
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentCopyChange {
    /// New child incarnation record.
    pub record: DocumentRecord,
    /// Owning child conversation.
    pub conversation_id: ConversationId,
    /// Exact persisted source.
    pub source: DocumentCopySource,
}

/// One synchronous post-adoption publication delivered to commit listeners.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitPublication {
    /// Commit sequence assigned by storage.
    pub seq: Seq,
    /// Table writes in staging order followed by document changes.
    pub changes: Vec<CommitChange>,
}

/// When one commit reaches Storage.
#[derive(Debug, Clone, PartialEq)]
pub enum CommitPersistence {
    Immediate,
    Deferred {
        /// `None` uses the Session default; infinity waits for a read/write or close.
        within_ms: Option<f64>,
    },
}

/// Options of one Session commit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommitOptions {
    pub persistence: Option<CommitPersistence>,
}

pub(crate) struct DeferredCommit {
    pub id: u64,
    pub deadline: Option<f64>,
    pub deadlines: std::collections::HashMap<crate::ids::DocumentId, Option<f64>>,
    pub writes: Vec<StorageWrite>,
    pub changes: Vec<CommitChange>,
    pub keys: Vec<String>,
    /// Documents held by this deferred commit; never evicted until flushed.
    pub pinned: Vec<crate::ids::DocumentId>,
    /// Documents whose held operations overflowed into a replacement: the
    /// stored change is a base and later held operations are not folded into
    /// it (upstream `Hold.pending`'s `replace` state).
    pub replaces: std::collections::HashSet<crate::ids::DocumentId>,
}

pub(crate) fn earliest_deferred_deadline(
    deadlines: &std::collections::HashMap<crate::ids::DocumentId, Option<f64>>,
) -> Option<f64> {
    deadlines
        .values()
        .copied()
        .fold(None, |earliest, deadline| match (earliest, deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        })
}

/// Synchronous observer of value calls and one terminal end.
pub trait Observer<T>: Send + Sync {
    fn advance(&self, call: WatchCall<T>, context: &crate::chord::context::Context);
    fn end(&self, end: WatchEnd);
}

/// Atomic observation attachment and its initial value.
pub struct Observation<T> {
    pub value: T,
    pub detach: Subscription,
}

/// Synchronous observer of stored commit publications.
pub trait CommitObserver: Send + Sync {
    fn advance(&self, publication: &CommitPublication, context: &crate::chord::context::Context);
    fn end(&self, end: WatchEnd);
}

/// Commit observation has no state value: commits are events, not a snapshot.
pub struct CommitObservation {
    pub detach: Subscription,
}

/// Operations an observer applies for one committed change (upstream
/// `observedOperations`, `session.ts:557`).
///
/// A retirement is a root replacement with `null`; a same-version change carries
/// its exact operations; a version change is delivered as a root replacement
/// because the observer holds a differently shaped value.
pub fn observed_operations(
    observed_version: &std::sync::atomic::AtomicU64,
    change: &DocumentChange,
) -> Vec<Op> {
    use std::sync::atomic::Ordering;
    if change.is_retirement() {
        return RETIREMENT_OPERATIONS.to_vec();
    }
    let version = change.version.unwrap_or(0);
    if version == observed_version.load(Ordering::SeqCst) {
        return change.ops.clone();
    }
    observed_version.store(version, Ordering::SeqCst);
    vec![Op::Replace(change.frame_value())]
}

/// Like [`observed_operations`], but also treats a new document incarnation
/// as a root replacement. A watch follows the address, so a retire/recreate
/// must not deliver the new value with the old incarnation's (empty) ops.
pub fn observed_operations_for(
    observed_id: &std::sync::atomic::AtomicU64,
    observed_version: &std::sync::atomic::AtomicU64,
    change: &DocumentChange,
) -> Vec<Op> {
    use std::sync::atomic::Ordering;
    if change.is_retirement() {
        return RETIREMENT_OPERATIONS.to_vec();
    }
    let id = change.record.id.get();
    let version = change.version.unwrap_or(0);
    if id != observed_id.load(Ordering::SeqCst) {
        observed_id.store(id, Ordering::SeqCst);
        observed_version.store(version, Ordering::SeqCst);
        return vec![Op::Replace(change.frame_value())];
    }
    observed_operations(observed_version, change)
}

/// Default wall clock (upstream `Date.now`).
pub fn default_now() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(0.0)
}
