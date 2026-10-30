//! Live task graph mount (port of `packages/durable/src/harness/task-graph.ts`
//! @ `4a080252f`).
//!
//! The task graph is a Chord replicated state over every live (non-terminal)
//! task: its durable status without checkpoint/outcome payloads, its owner edge,
//! and the conversations it owns in ID order. The mount is built on the Session
//! line by its first observer and dropped with its last, advancing from the
//! Session's commit publications.
//!
//! Rust shape note (registered V17-14 §7.3): upstream applies a hand-built `Op`
//! batch; the Rust port recomputes the changed node and publishes
//! `diff_json(previous, next)` (the tracker's structural-diff shape). A commit
//! that changes nothing publishes no revision.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::chord::context::{Context, without_abort_signal};
use crate::chord::delta::diff_json;
use crate::chord::json::JsonValue;
use crate::chord::state::AttachedReplicatedState;
use crate::harness::scheduler::{BlockedListener, BlockedSubject, SchedulerService};

/// Derives one task's blocked reason for the graph.
pub type BlockedFn = Arc<dyn Fn(&BlockedSubject) -> Option<BlockedReason> + Send + Sync>;
use crate::harness::types::BlockedReason;
use crate::ids::{ConversationId, TaskId};
use crate::session::observation::{CommittedStateSource, CommittedWatch};
use crate::session::{
    CommitChange, CommitPublication, SessionError, SessionImpl, SessionResult, Subscription,
};
use crate::types::{
    ConversationQuery, ConversationRecord, Page, Storage, StorageWrite, TaskOutcome, TaskQuery,
    TaskRecord, TaskState, TaskStatus,
};

/// A live task's durable status without its checkpoint and outcome payloads
/// (spec §9.5).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskGraphState {
    /// Eligible for scheduling.
    Pending {
        /// Current checkpoint phase.
        phase: String,
    },
    /// Reserved by an invocation.
    Running {
        /// Current checkpoint phase.
        phase: String,
    },
    /// Sleeping until the Harness clock reaches a deadline.
    Sleeping {
        /// Current checkpoint phase.
        phase: String,
        /// Deadline on the harness clock.
        until: f64,
    },
    /// Parked on other tasks.
    Waiting {
        /// Current checkpoint phase.
        phase: String,
        /// Waited task IDs.
        on: Vec<TaskId>,
        /// How the wait reacts to failures.
        policy: crate::types::JoinPolicy,
    },
    /// Outcome held until ordinary owned work drains.
    Completing {
        /// Held outcome status.
        outcome: String,
    },
}

/// One live task of the graph.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGraphNode {
    /// Task identity.
    pub id: TaskId,
    /// Registered task kind.
    pub kind: String,
    /// Conversation the task lives in.
    pub conversation_id: ConversationId,
    /// Owning task of a child task; absent for a conversation-owned task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    /// Whether the task is excluded from ordinary idle waits.
    pub background: bool,
    /// Durable abort mark.
    pub abort_requested: bool,
    /// Durable status without payloads.
    pub state: TaskGraphState,
    /// Why it cannot run here, when it cannot: no registered definition takes it
    /// (`missing_task`, `task_too_old`, `migration_failed`), or the restart
    /// limit parked it (`restart_limit`). Follows registry changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
    /// Conversations this task owns, in ID order.
    pub conversations: Vec<ConversationId>,
}

/// Every live task of the Session (spec §9.5), keyed by decimal ID.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TaskGraph {
    /// Every live task, keyed by its decimal ID.
    pub tasks: BTreeMap<String, TaskGraphNode>,
}

/// A serialized exact-frame watch of the graph (upstream `TaskGraphWatch`).
pub type TaskGraphWatch = CommittedWatch;

const LIVE_STATUSES: [TaskStatus; 5] = [
    TaskStatus::Pending,
    TaskStatus::Running,
    TaskStatus::Waiting,
    TaskStatus::Sleeping,
    TaskStatus::Completing,
];
const SCAN_PAGE_SIZE: usize = 256;

enum Observer {
    State(Arc<CommittedStateSource>),
    Watch(u64, CommittedWatch),
}

struct Mount {
    value: TaskGraph,
    /// What the blocked reason reads of each live task, so a registry change can
    /// derive it again without payloads.
    records: BTreeMap<String, BlockedSubject>,
    observers: Vec<Observer>,
}

/// The Harness's task graph mount: built on the Session line by its first
/// observer and dropped with its last.
pub struct TaskGraphView {
    session: Arc<SessionImpl>,
    storage: Arc<dyn Storage>,
    blocked: BlockedFn,
    blocked_subscription: Mutex<Option<Subscription>>,
    mount: Mutex<Option<Mount>>,
    closed: AtomicBool,
    next_observer: AtomicU64,
}

impl TaskGraphView {
    /// Build the view and subscribe it to the session's commit and close
    /// publications.
    pub fn open(
        session: Arc<SessionImpl>,
        storage: Arc<dyn Storage>,
        scheduler: Arc<dyn SchedulerService>,
    ) -> Arc<Self> {
        let blocking = Arc::clone(&scheduler);
        let view = Arc::new(Self {
            session: Arc::clone(&session),
            storage,
            blocked: Arc::new(move |subject: &BlockedSubject| blocking.blocked_reason(subject)),
            blocked_subscription: Mutex::new(None),
            mount: Mutex::new(None),
            closed: AtomicBool::new(false),
            next_observer: AtomicU64::new(1),
        });
        let advancing = Arc::clone(&view);
        session.subscribe_commits(Arc::new(move |publication, context| {
            advancing.on_publication(publication, context);
        }));
        let closing = Arc::clone(&view);
        session.subscribe_close(Arc::new(move || {
            closing.close_session();
        }));
        let refreshing = Arc::clone(&view);
        let listener: BlockedListener = Arc::new(move || refreshing.refresh_blocked());
        let subscription = scheduler.subscribe_blocked(listener);
        if let Ok(mut slot) = view.blocked_subscription.lock() {
            *slot = Some(subscription);
        }
        view
    }

    fn on_publication(&self, publication: &CommitPublication, context: &Context) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut guard) = self.mount.lock() else {
            return;
        };
        let Some(mount) = guard.as_mut() else {
            return;
        };
        advance(mount, publication, context, &*self.blocked);
    }

    fn close_session(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = self.blocked_subscription.lock()
            && let Some(subscription) = slot.take()
        {
            subscription.unsubscribe();
        }
        let observers = match self.mount.lock() {
            Ok(mut guard) => guard.take().map(|mount| mount.observers),
            Err(_) => None,
        };
        let failure = self.session.failure();
        for observer in observers.into_iter().flatten() {
            match observer {
                Observer::State(source) => source.close_session(failure.clone()),
                Observer::Watch(_, watch) => watch.close_session(failure.clone()),
            }
        }
    }

    /// A disposable read-only Chord state of the graph.
    pub async fn state(
        self: &Arc<Self>,
        context: &Context,
    ) -> SessionResult<AttachedReplicatedState> {
        let this = Arc::clone(self);
        let read_context = context.clone();
        self.session
            .read_on_line(move || {
                let this = Arc::clone(&this);
                Box::pin(async move { this.attach_state(&read_context).await })
            })
            .await
    }

    /// A serialized exact-frame watch of the graph; cancelling `context` stops it.
    pub async fn watch(self: &Arc<Self>, context: &Context) -> SessionResult<TaskGraphWatch> {
        let this = Arc::clone(self);
        let read_context = context.clone();
        self.session
            .read_on_line(move || {
                let this = Arc::clone(&this);
                Box::pin(async move { this.attach_watch(&read_context).await })
            })
            .await
    }

    async fn attach_state(
        self: &Arc<Self>,
        context: &Context,
    ) -> SessionResult<AttachedReplicatedState> {
        if is_cancelled(context) {
            return Err(SessionError::Aborted);
        }
        let value = self.mount_value(context).await?;
        // Close or cancellation may begin while the mount builds.
        if self.closed.load(Ordering::SeqCst) {
            return Err(SessionError::message("Harness is closed"));
        }
        if is_cancelled(context) {
            return Err(SessionError::Aborted);
        }
        let source = Arc::new(CommittedStateSource::new(graph_json(&value)));
        let this = Arc::clone(self);
        let released = Arc::clone(&source);
        source.set_release(Box::new(move || this.detach_state(&released)));
        let state = crate::session::observation::attach_state(&source);
        self.mount_observer(Observer::State(source))?;
        Ok(state)
    }

    async fn attach_watch(self: &Arc<Self>, context: &Context) -> SessionResult<TaskGraphWatch> {
        if is_cancelled(context) {
            return Err(SessionError::Aborted);
        }
        let value = self.mount_value(context).await?;
        if self.closed.load(Ordering::SeqCst) {
            return Err(SessionError::message("Harness is closed"));
        }
        if is_cancelled(context) {
            return Err(SessionError::Aborted);
        }
        let watch = CommittedWatch::new(graph_json(&value));
        let watch_id = self.next_observer.fetch_add(1, Ordering::Relaxed);
        let this = Arc::clone(self);
        watch.set_detach(Box::new(move || this.detach_watch(watch_id)));
        self.mount_observer(Observer::Watch(watch_id, watch.clone()))?;
        if let Some(signal) = context.abort_signal() {
            if signal.is_cancelled() {
                watch.cancel();
                return Err(SessionError::Aborted);
            }
            let cancelling = watch.clone();
            tokio::spawn(async move {
                signal.cancelled().await;
                cancelling.cancel();
            });
        }
        Ok(watch)
    }

    async fn mount_value(self: &Arc<Self>, context: &Context) -> SessionResult<TaskGraph> {
        if let Ok(guard) = self.mount.lock()
            && let Some(mount) = guard.as_ref()
        {
            return Ok(mount.value.clone());
        }
        let (value, records) = self.build(context).await?;
        let mut guard = self
            .mount
            .lock()
            .map_err(|_| SessionError::message("Task graph mount lock poisoned"))?;
        match guard.as_ref() {
            Some(mount) => Ok(mount.value.clone()),
            None => {
                *guard = Some(Mount {
                    value: value.clone(),
                    records,
                    observers: Vec::new(),
                });
                Ok(value)
            }
        }
    }

    fn mount_observer(&self, observer: Observer) -> SessionResult<()> {
        let mut guard = self
            .mount
            .lock()
            .map_err(|_| SessionError::message("Task graph mount lock poisoned"))?;
        match guard.as_mut() {
            Some(mount) => {
                mount.observers.push(observer);
                Ok(())
            }
            None => Err(SessionError::message("Task graph mount disappeared")),
        }
    }

    fn detach_state(&self, source: &Arc<CommittedStateSource>) {
        self.detach(|observer| matches!(observer, Observer::State(existing) if Arc::ptr_eq(existing, source)));
    }

    fn detach_watch(&self, id: u64) {
        self.detach(|observer| matches!(observer, Observer::Watch(existing, _) if *existing == id));
    }

    fn detach(&self, matches: impl Fn(&Observer) -> bool) {
        let Ok(mut guard) = self.mount.lock() else {
            return;
        };
        let Some(mount) = guard.as_mut() else {
            return;
        };
        mount.observers.retain(|observer| !matches(observer));
        if mount.observers.is_empty() {
            *guard = None;
        }
    }

    /// On the Session line: derive every node's blocked reason again, and
    /// publish the ones that changed.
    fn refresh_blocked(self: &Arc<Self>) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        // Do not probe `mount` here: a host callback may call this while
        // `publish` holds the mount lock. The spawned refresh rechecks it.
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let refresh = Arc::clone(&this);
            let _ = this
                .session
                .read_on_line(move || {
                    Box::pin(async move {
                        refresh.apply_blocked_refresh();
                        Ok(())
                    })
                })
                .await;
        });
    }

    fn apply_blocked_refresh(&self) {
        let Ok(mut guard) = self.mount.lock() else {
            return;
        };
        let Some(mount) = guard.as_mut() else {
            return;
        };
        let mut next = mount.value.clone();
        let mut changed = false;
        for (key, subject) in &mount.records {
            let blocked = (self.blocked)(subject);
            if let Some(node) = next.tasks.get_mut(key)
                && node.blocked != blocked
            {
                node.blocked = blocked;
                changed = true;
            }
        }
        if changed {
            publish(mount, next, &crate::chord::context::BACKGROUND_CONTEXT);
        }
    }

    async fn build(
        &self,
        context: &Context,
    ) -> SessionResult<(TaskGraph, BTreeMap<String, BlockedSubject>)> {
        let mut records: Vec<TaskRecord> = Vec::new();
        for status in LIVE_STATUSES {
            let mut cursor = None;
            loop {
                let page = self
                    .storage
                    .scan_tasks(
                        TaskQuery {
                            status: Some(status),
                            ..TaskQuery::default()
                        },
                        SCAN_PAGE_SIZE,
                        cursor,
                        context,
                    )
                    .await?;
                records.extend(page.items);
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        records.sort_by_key(|record| record.id);
        let mut tasks: BTreeMap<String, TaskGraphNode> = BTreeMap::new();
        let mut subjects: BTreeMap<String, BlockedSubject> = BTreeMap::new();
        for record in &records {
            let owned = self.scan_owned(record.id, context).await?;
            let mut conversations: Vec<ConversationId> =
                owned.iter().map(|conversation| conversation.id).collect();
            conversations.sort();
            let key = record.id.get().to_string();
            let subject = BlockedSubject::of(record);
            tasks.insert(
                key.clone(),
                node_of(record, conversations, (self.blocked)(&subject)),
            );
            subjects.insert(key, subject);
        }
        Ok((TaskGraph { tasks }, subjects))
    }

    async fn scan_owned(
        &self,
        task_id: TaskId,
        context: &Context,
    ) -> SessionResult<Vec<ConversationRecord>> {
        let mut conversations = Vec::new();
        let mut cursor = None;
        loop {
            let Page { items, next }: Page<ConversationRecord> = self
                .storage
                .scan_conversations(
                    ConversationQuery {
                        owner_task_id: Some(task_id),
                        ..ConversationQuery::default()
                    },
                    SCAN_PAGE_SIZE,
                    cursor,
                    context,
                )
                .await?;
            conversations.extend(items);
            match next {
                Some(cursor_next) => cursor = Some(cursor_next),
                None => break,
            }
        }
        Ok(conversations)
    }
}

fn is_cancelled(context: &Context) -> bool {
    context
        .abort_signal()
        .is_some_and(|signal| signal.is_cancelled())
}

/// Derive the mount's new revision from one publication and hand it to every
/// observer.
// FORM: upstream applies a hand-built `Op` batch; the port recomputes the graph
// value and emits `diff_json(previous, next)` at the registered tracker shape.
fn advance(
    mount: &mut Mount,
    publication: &CommitPublication,
    context: &Context,
    blocked: &(dyn Fn(&BlockedSubject) -> Option<BlockedReason> + Send + Sync),
) {
    let mut next = mount.value.clone();
    for change in &publication.changes {
        if let CommitChange::Table(StorageWrite::Task { value }) = change {
            let key = value.id.get().to_string();
            let previous = next.tasks.get(&key).cloned();
            if value.status() == TaskStatus::Terminal {
                mount.records.remove(&key);
                if previous.is_some() {
                    next.tasks.remove(&key);
                }
                continue;
            }
            let subject = BlockedSubject::of(value);
            let node = node_of(
                value,
                previous
                    .as_ref()
                    .map(|node| node.conversations.clone())
                    .unwrap_or_default(),
                blocked(&subject),
            );
            mount.records.insert(key.clone(), subject);
            if previous.as_ref() == Some(&node) {
                continue;
            }
            next.tasks.insert(key, node);
        }
    }
    // After the tasks, so a conversation created with its owner task in one
    // commit finds the owner's node. Change order within a publication is
    // unspecified, so each owner's list is sorted again.
    for change in &publication.changes {
        if let CommitChange::Table(StorageWrite::Conversation { value }) = change
            && let Some(owner) = &value.owner
        {
            let key = owner.task_id.get().to_string();
            if let Some(node) = next.tasks.get_mut(&key)
                && !node.conversations.contains(&value.id)
            {
                node.conversations.push(value.id);
                node.conversations.sort();
            }
        }
    }
    publish(mount, next, context);
}

/// Apply a recomputed graph and hand the revision to every observer.
fn publish(mount: &mut Mount, next: TaskGraph, context: &Context) {
    if next == mount.value {
        return;
    }
    let ops = diff_json(&graph_json(&mount.value), &graph_json(&next));
    mount.value = next;
    let value = graph_json(&mount.value);
    let frame_context = without_abort_signal(context);
    for observer in &mount.observers {
        match observer {
            Observer::State(source) => {
                source.advance(value.clone(), ops.clone(), frame_context.clone())
            }
            Observer::Watch(_, watch) => {
                watch.advance(value.clone(), ops.clone(), frame_context.clone())
            }
        }
    }
}

fn node_of(
    record: &TaskRecord,
    conversations: Vec<ConversationId>,
    blocked: Option<BlockedReason>,
) -> TaskGraphNode {
    TaskGraphNode {
        id: record.id,
        kind: record.kind.clone(),
        conversation_id: record.conversation_id,
        owner: record.owner,
        background: record.background,
        abort_requested: record.abort_requested,
        state: state_of(record),
        blocked,
        conversations,
    }
}

fn state_of(record: &TaskRecord) -> TaskGraphState {
    match &record.state {
        TaskState::Pending { checkpoint } => TaskGraphState::Pending {
            phase: phase_of(checkpoint),
        },
        TaskState::Running { checkpoint } => TaskGraphState::Running {
            phase: phase_of(checkpoint),
        },
        TaskState::Waiting {
            checkpoint,
            on,
            policy,
        } => TaskGraphState::Waiting {
            phase: phase_of(checkpoint),
            on: on.clone(),
            policy: *policy,
        },
        TaskState::Sleeping { checkpoint, until } => TaskGraphState::Sleeping {
            phase: phase_of(checkpoint),
            until: *until,
        },
        // Terminal records never reach here: they leave the graph.
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            TaskGraphState::Completing {
                outcome: outcome_status(outcome),
            }
        }
    }
}

fn outcome_status(outcome: &TaskOutcome) -> String {
    match outcome {
        TaskOutcome::Completed { .. } => "completed",
        TaskOutcome::Failed { .. } => "failed",
        TaskOutcome::Aborted { .. } => "aborted",
        TaskOutcome::Orphaned { .. } => "orphaned",
        TaskOutcome::Faulted { .. } => "faulted",
    }
    .to_string()
}

fn phase_of(checkpoint: &JsonValue) -> String {
    checkpoint
        .get("phase")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string()
}

fn graph_json(graph: &TaskGraph) -> JsonValue {
    // Invariant: `TaskGraph` always serializes to a JSON object.
    serde_json::to_value(graph).unwrap_or(JsonValue::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        ConversationOwnership, TaskCreate, TaskOwnership, commit_fn, create_session,
    };
    use crate::storage::MemoryStorage;

    fn context() -> Context {
        crate::chord::context::BACKGROUND_CONTEXT.clone()
    }

    async fn create_task(
        session: &Arc<SessionImpl>,
        conversation: ConversationId,
        kind: &str,
        background: bool,
    ) -> TaskId {
        let kind = kind.to_string();
        session
            .commit(
                commit_fn(move |tx| {
                    let kind = kind.clone();
                    Box::pin(async move {
                        tx.create_task(TaskCreate {
                            kind,
                            version: 1,
                            input: JsonValue::Null,
                            checkpoint: serde_json::json!({ "phase": "prepare" }),
                            ownership: TaskOwnership::Conversation(conversation),
                            conversation_id: Some(conversation),
                            background,
                            abandon_on_restart: false,
                            trace: Default::default(),
                            links: Vec::new(),
                        })
                        .await
                    })
                }),
                &context(),
            )
            .await
            .expect("create task")
    }

    #[tokio::test]
    async fn follows_task_status_and_publishes_no_noop_revision() {
        let session = create_session(Arc::new(MemoryStorage::new()), None);
        let storage: Arc<dyn Storage> = Arc::clone(session.storage());
        let view = TaskGraphView::open(
            Arc::clone(&session),
            storage,
            Arc::new(crate::harness::scheduler::UnavailableScheduler::new()),
        );
        let conversation = session
            .commit(
                commit_fn(|tx| {
                    Box::pin(async move {
                        Ok(tx
                            .create_conversation(ConversationOwnership::Ownerless)
                            .await?
                            .id)
                    })
                }),
                &context(),
            )
            .await
            .expect("conversation");
        let task = create_task(&session, conversation, "pi.generation", false).await;
        let state = view.state(&context()).await.expect("state");
        let value = state.value();
        assert_eq!(
            value["tasks"][task.get().to_string()]["state"]["status"],
            "pending"
        );
        assert_eq!(
            value["tasks"][task.get().to_string()]["kind"],
            "pi.generation"
        );
        // A no-op commit publishes no revision.
        session
            .commit(
                commit_fn(move |_tx| Box::pin(async move { Ok::<(), SessionError>(()) })),
                &context(),
            )
            .await
            .expect("noop");
        assert_eq!(state.value(), value);
        state.dispose();
    }

    #[tokio::test]
    async fn lists_owned_conversations_in_id_order() {
        let session = create_session(Arc::new(MemoryStorage::new()), None);
        let storage: Arc<dyn Storage> = Arc::clone(session.storage());
        let view = TaskGraphView::open(
            Arc::clone(&session),
            storage,
            Arc::new(crate::harness::scheduler::UnavailableScheduler::new()),
        );
        let context = context();
        let root = session
            .commit(
                commit_fn(|tx| {
                    Box::pin(async move {
                        Ok(tx
                            .create_conversation(ConversationOwnership::Ownerless)
                            .await?
                            .id)
                    })
                }),
                &context,
            )
            .await
            .expect("root");
        let task = create_task(&session, root, "pi.generation", false).await;
        // Two conversations owned by the task; the mount lists them sorted by ID.
        let ids = session
            .commit(
                commit_fn(move |tx| {
                    Box::pin(async move {
                        let first = tx
                            .create_conversation(ConversationOwnership::Task(task))
                            .await?;
                        let second = tx
                            .create_conversation(ConversationOwnership::Task(task))
                            .await?;
                        Ok(vec![first.id, second.id])
                    })
                }),
                &context,
            )
            .await
            .expect("owned");
        let mut sorted = ids.clone();
        sorted.sort();
        let state = view.state(&context).await.expect("state");
        let conversations = state.value()["tasks"][task.get().to_string()]["conversations"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let actual: Vec<u64> = conversations.iter().filter_map(JsonValue::as_u64).collect();
        let expected: Vec<u64> = sorted.iter().map(|id| id.get()).collect();
        assert_eq!(actual, expected);
        state.dispose();
    }

    #[tokio::test]
    async fn shares_one_mount_between_observers() {
        let session = create_session(Arc::new(MemoryStorage::new()), None);
        let storage: Arc<dyn Storage> = Arc::clone(session.storage());
        let view = TaskGraphView::open(
            Arc::clone(&session),
            storage,
            Arc::new(crate::harness::scheduler::UnavailableScheduler::new()),
        );
        let first = view.state(&context()).await.expect("first");
        let second = view.state(&context()).await.expect("second");
        assert_eq!(first.value(), second.value());
        first.dispose();
        second.dispose();
        assert!(view.mount.lock().expect("mount").is_none());
    }
}
