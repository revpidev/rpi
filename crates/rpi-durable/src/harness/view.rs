//! Conversation view mounts and per-observer bounded transcript windows.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::chord::context::Context;
use crate::chord::delta::Op;
use crate::chord::json::{JsonObject, JsonValue};
use crate::harness::{AGENT_DOC, INBOX_DOC, LIVE_DOC, PROVIDER_DOC, USAGE_DOC};
use crate::ids::{ConversationId, DocumentId, EntryId, id_from_number};
use crate::session::{
    CommitPublication, CommittedWatch, Observation, Observer, SessionError, SessionFuture,
    SessionResult, Subscription, WatchCall, WatchEnd, WatchFuture,
};
use crate::types::{
    ConversationRecord, Cursor, DocumentScope, EntryQuery, EntryRecord, Page, ScanOrder,
    StorageWrite,
};

/// Structural conversation view with fork-aware entries and built-in documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationView {
    pub conversation: ConversationRecord,
    pub entries: Vec<EntryRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<EntryId>,
    pub docs: BTreeMap<String, JsonObject>,
}

/// Bounds of the initial window and the append retention policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConversationWindow {
    pub from: Option<EntryId>,
    pub min: Option<usize>,
    pub max: Option<usize>,
}

impl ConversationWindow {
    fn resolve(self) -> SessionResult<(Option<EntryId>, usize, usize)> {
        let min = self.min.unwrap_or(50);
        if min < 1 {
            return Err(SessionError::message("Window min must be an integer >= 1"));
        }
        let max = self.max.unwrap_or(min.saturating_mul(2));
        if max < min {
            return Err(SessionError::message(format!(
                "Window max must be an integer >= min ({min})"
            )));
        }
        Ok((self.from, min, max))
    }
}

/// Serialized listener of a conversation view.
pub type ConversationWatchListener =
    Arc<dyn Fn(WatchCall<ConversationView>, Context) -> WatchFuture + Send + Sync>;

/// Async view watch backed by the shared Chord frame queue.
pub struct ConversationWatch {
    inner: CommittedWatch,
}

impl ConversationWatch {
    pub fn start(&self, listener: ConversationWatchListener) -> SessionResult<()> {
        self.inner.start(Arc::new(move |call, context| {
            let listener = Arc::clone(&listener);
            Box::pin(async move {
                let call: WatchCall<ConversationView> = match call {
                    WatchCall::Value {
                        value,
                        ops,
                        retired,
                    } => {
                        let value = serde_json::from_value(
                            value.map(JsonValue::Object).expect("document frame"),
                        )
                        .expect("conversation view frame");
                        WatchCall::Value {
                            value,
                            ops,
                            retired,
                        }
                    }
                    WatchCall::Failed { error } => WatchCall::Failed { error },
                };
                listener(call, context).await
            })
        }))
    }

    pub async fn stop(&self) -> WatchEnd {
        self.inner.stop().await
    }

    pub async fn closed(&self) -> WatchEnd {
        self.inner.closed().await
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn value(&self) -> Option<ConversationView> {
        self.inner
            .value()
            .map(JsonValue::Object)
            .and_then(|value| serde_json::from_value(value).ok())
    }
}

struct Windowed {
    min: usize,
    max: usize,
    value: ConversationView,
    watch: Option<CommittedWatch>,
    observer: Option<Arc<dyn Observer<ConversationView>>>,
    reporter: Option<crate::session::session::FailureReporter>,
    alive: bool,
}

impl Windowed {
    fn advance(&mut self, view: ConversationView, context: &Context) {
        self.value = view.clone();
        if let Some(watch) = &self.watch {
            let value = serde_json::to_value(&view).unwrap_or(JsonValue::Null);
            watch.advance(
                value.clone(),
                vec![Op::Replace(value)],
                false,
                context.clone(),
            );
            return;
        }
        let Some(observer) = &self.observer else {
            return;
        };
        let succeeded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.advance(
                WatchCall::Value {
                    value: view.clone(),
                    ops: vec![Op::Replace(
                        serde_json::to_value(&view).unwrap_or(JsonValue::Null),
                    )],
                    retired: false,
                },
                context,
            );
        }))
        .is_ok();
        if !succeeded {
            if let Some(reporter) = &self.reporter {
                reporter(
                    "listener_error: conversation observer panicked".to_string(),
                    Some(context.clone()),
                );
            }
            self.end(WatchEnd::ListenerError {
                error: "conversation observer panicked".to_string(),
            });
        }
    }

    fn end(&mut self, end: WatchEnd) {
        if !self.alive {
            return;
        }
        self.alive = false;
        if let Some(watch) = &self.watch {
            match end {
                WatchEnd::Failed { error } => watch.close_session(Some(error)),
                _ => watch.close_session(None),
            }
        } else if let Some(observer) = &self.observer {
            observer.end(end);
        }
    }
}

struct Mount {
    conversation: ConversationRecord,
    docs: BTreeMap<String, JsonObject>,
    /// Current incarnation of each mounted document, by kind. A watch follows
    /// the address, so retire/recreate must replace rather than remove the
    /// entry (upstream `Mount.incarnations`, view.ts:256-262).
    incarnations: BTreeMap<String, (DocumentId, u64)>,
    observers: HashMap<u64, Windowed>,
}

/// The built-in documents a conversation view mounts (upstream `MOUNTED`).
const MOUNTED_KINDS: [&str; 5] = ["pi.agent", "pi.live", "pi.inbox", "pi.provider", "pi.usage"];

impl Mount {
    fn advance(&mut self, publication: &CommitPublication, context: &Context) {
        let mut appended = Vec::new();
        let mut documents_changed = false;
        for change in &publication.changes {
            match change {
                crate::session::CommitChange::Document(document) => {
                    if document.record.scope
                        != (DocumentScope::Conversation {
                            conversation_id: self.conversation.id,
                        })
                        || !MOUNTED_KINDS.contains(&document.record.kind.as_str())
                        || document.record.key.is_some()
                    {
                        continue;
                    }
                    let kind = document.record.kind.clone();
                    let version = document.version.unwrap_or(0);
                    let current = self.incarnations.get(&kind).copied();
                    if document.is_retirement() {
                        if current.map(|(id, _)| id) == Some(document.record.id) {
                            self.incarnations.remove(&kind);
                            self.docs.remove(&kind);
                            documents_changed = true;
                        }
                    } else if let Some(value) = &document.value {
                        self.incarnations
                            .insert(kind.clone(), (document.record.id, version));
                        self.docs.insert(kind, value.clone());
                        documents_changed = true;
                    }
                }
                crate::session::CommitChange::Table(StorageWrite::Entry { value })
                    if value.conversation_id == self.conversation.id =>
                {
                    appended.push(value.clone());
                }
                _ => {}
            }
        }
        if appended.is_empty() && !documents_changed {
            return;
        }
        for windowed in self.observers.values_mut() {
            for entry in &appended {
                if !windowed
                    .value
                    .entries
                    .iter()
                    .any(|held| held.id == entry.id)
                {
                    windowed.value.entries.push(entry.clone());
                    windowed.value.entries.sort_by_key(|entry| entry.id);
                }
            }
            if windowed.value.entries.len() > windowed.max {
                let drop = windowed.value.entries.len() - windowed.min;
                windowed.value.before = windowed.value.entries.get(drop - 1).map(|entry| entry.id);
                windowed.value.entries.drain(..drop);
            }
            windowed.value.docs = self.docs.clone();
            windowed.advance(windowed.value.clone(), context);
        }
        self.observers.retain(|_, windowed| windowed.alive);
    }
}

/// Conversation mounts owned by a Harness, dropped with their last observer.
pub struct ConversationViews {
    session: Arc<crate::session::SessionImpl>,
    mounts: Arc<Mutex<HashMap<ConversationId, Mount>>>,
    next_observer: AtomicU64,
    _commit: Subscription,
    _close: Subscription,
}

struct Attachment {
    value: ConversationView,
    watch: Option<CommittedWatch>,
    detach: Subscription,
}

impl ConversationViews {
    pub fn new(session: Arc<crate::session::SessionImpl>) -> Arc<Self> {
        let mounts = Arc::new(Mutex::new(HashMap::<ConversationId, Mount>::new()));
        let commit_mounts = Arc::clone(&mounts);
        let commit = session.subscribe_internal_commits(Arc::new(move |publication, context| {
            if let Ok(mut mounts) = commit_mounts.lock() {
                for mount in mounts.values_mut() {
                    mount.advance(publication, context);
                }
            }
        }));
        let close_mounts = Arc::clone(&mounts);
        let weak = Arc::downgrade(&session);
        let close = session.subscribe_close(Arc::new(move || {
            let failure = weak.upgrade().and_then(|session| session.failure());
            if let Ok(mut mounts) = close_mounts.lock() {
                for mount in mounts.values_mut() {
                    for windowed in mount.observers.values_mut() {
                        windowed.end(match (&failure, windowed.observer.is_some()) {
                            (Some(error), _) => WatchEnd::Failed {
                                error: error.clone(),
                            },
                            (None, true) => WatchEnd::Failed {
                                error: "Session is closed".to_string(),
                            },
                            (None, false) => WatchEnd::Ended,
                        });
                    }
                }
                mounts.clear();
            }
        }));
        Arc::new(Self {
            session,
            mounts,
            next_observer: AtomicU64::new(1),
            _commit: commit,
            _close: close,
        })
    }

    pub fn watch(
        self: &Arc<Self>,
        id: ConversationId,
        window: ConversationWindow,
        context: &Context,
    ) -> SessionFuture<'_, ConversationWatch> {
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move {
            let signal = context.abort_signal();
            let attachment = this.attach(id, window, None, context).await?;
            if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                attachment.detach.unsubscribe();
                return Err(SessionError::Aborted);
            }
            let inner = attachment.watch.expect("watch attachment");
            if let Some(signal) = signal {
                let watch = inner.clone();
                tokio::spawn(async move {
                    signal.cancelled().await;
                    watch.cancel();
                });
            }
            Ok(ConversationWatch { inner })
        })
    }

    pub fn observe(
        self: &Arc<Self>,
        id: ConversationId,
        observer: Arc<dyn Observer<ConversationView>>,
        window: ConversationWindow,
        context: &Context,
    ) -> SessionFuture<'_, Observation<ConversationView>> {
        let this = Arc::clone(self);
        let context = context.clone();
        Box::pin(async move {
            let signal = context.abort_signal();
            let attachment = this.attach(id, window, Some(observer), context).await?;
            if signal.is_some_and(|signal| signal.is_cancelled()) {
                attachment.detach.unsubscribe();
                return Err(SessionError::Aborted);
            }
            Ok(Observation {
                value: attachment.value,
                detach: attachment.detach,
            })
        })
    }

    async fn attach(
        self: &Arc<Self>,
        id: ConversationId,
        window: ConversationWindow,
        observer: Option<Arc<dyn Observer<ConversationView>>>,
        context: Context,
    ) -> SessionResult<Attachment> {
        let (from, min, max) = window.resolve()?;
        let this = Arc::clone(self);
        self.session
            .read_on_line(move || {
                Box::pin(async move {
                    let (conversation, docs, incarnations, entries, before) =
                        this.build(id, from, min, max, &context).await?;
                    let value = ConversationView {
                        conversation,
                        entries,
                        before,
                        docs,
                    };
                    let watch = observer.is_none().then(|| {
                        CommittedWatch::new(serde_json::to_value(&value).unwrap_or(JsonValue::Null))
                    });
                    let id_for_detach = id;
                    let observer_id = this.next_observer.fetch_add(1, Ordering::SeqCst);
                    let reporter = observer.as_ref().map(|_| {
                        let session = Arc::downgrade(&this.session);
                        let reporter: crate::session::session::FailureReporter =
                            Arc::new(move |message, context| {
                                if let Some(session) = session.upgrade() {
                                    session.report_listener_error(message, context);
                                }
                            });
                        reporter
                    });
                    let windowed = Windowed {
                        min,
                        max,
                        value: value.clone(),
                        watch: watch.clone(),
                        observer,
                        reporter,
                        alive: true,
                    };
                    if let Ok(mut mounts) = this.mounts.lock() {
                        let mount = mounts.entry(id_for_detach).or_insert_with(|| Mount {
                            conversation: value.conversation.clone(),
                            docs: value.docs.clone(),
                            incarnations: incarnations.clone(),
                            observers: HashMap::new(),
                        });
                        mount.conversation = value.conversation.clone();
                        mount.docs = value.docs.clone();
                        mount.incarnations = incarnations;
                        mount.observers.insert(observer_id, windowed);
                    }
                    let mounts = Arc::clone(&this.mounts);
                    Ok(Attachment {
                        value,
                        watch: watch.clone(),
                        detach: Subscription::new(move || {
                            if let Some(watch) = &watch {
                                watch.close_session(None);
                            }
                            if let Ok(mut mounts) = mounts.lock() {
                                let empty = mounts
                                    .get_mut(&id_for_detach)
                                    .map(|mount| {
                                        if let Some(mut windowed) =
                                            mount.observers.remove(&observer_id)
                                        {
                                            windowed.end(WatchEnd::Stopped);
                                        }
                                        mount.observers.is_empty()
                                    })
                                    .unwrap_or(false);
                                if empty {
                                    mounts.remove(&id_for_detach);
                                }
                            }
                        }),
                    })
                })
            })
            .await
    }

    async fn build(
        &self,
        id: ConversationId,
        from: Option<EntryId>,
        min: usize,
        max: usize,
        context: &Context,
    ) -> SessionResult<(
        ConversationRecord,
        BTreeMap<String, JsonObject>,
        BTreeMap<String, (DocumentId, u64)>,
        Vec<EntryRecord>,
        Option<EntryId>,
    )> {
        let conversation = self
            .session
            .storage()
            .conversation(id, context)
            .await?
            .ok_or_else(|| SessionError::message(format!("Conversation {id} does not exist")))?;
        let mut docs = BTreeMap::new();
        let mut incarnations = BTreeMap::new();
        for token in [
            &*AGENT_DOC,
            &*LIVE_DOC,
            &*INBOX_DOC,
            &*PROVIDER_DOC,
            &*USAGE_DOC,
        ] {
            if let Some(document) = self
                .session
                .conversation_document_on_line(token, id, context)
                .await?
            {
                docs.insert(token.definition.kind.clone(), document.value);
                incarnations.insert(
                    token.definition.kind.clone(),
                    (document.record.id, document.version),
                );
            }
        }
        let (entries, before) =
            load_window(self.session.storage(), id, from, min, max, context).await?;
        Ok(((*conversation).clone(), docs, incarnations, entries, before))
    }
}

async fn scan_up_to(
    storage: &Arc<dyn crate::types::Storage>,
    query: EntryQuery,
    limit: usize,
    context: &Context,
) -> SessionResult<Vec<EntryRecord>> {
    let mut entries = Vec::new();
    let mut cursor: Option<Cursor> = None;
    while entries.len() < limit {
        let Page { items, next } = storage
            .scan_entries(
                query.clone(),
                (limit - entries.len()).min(256),
                cursor,
                context,
            )
            .await?;
        entries.extend(items);
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    Ok(entries)
}

async fn load_window(
    storage: &Arc<dyn crate::types::Storage>,
    id: ConversationId,
    from: Option<EntryId>,
    min: usize,
    max: usize,
    context: &Context,
) -> SessionResult<(Vec<EntryRecord>, Option<EntryId>)> {
    if let Some(from) = from {
        let entries = scan_up_to(
            storage,
            EntryQuery {
                conversation_id: id,
                min_entry_id: Some(from),
                max_entry_id: None,
                order: Some(ScanOrder::Ascending),
            },
            max.saturating_add(1),
            context,
        )
        .await?;
        if entries.len() <= max {
            let previous = storage
                .scan_entries(
                    EntryQuery {
                        conversation_id: id,
                        min_entry_id: None,
                        max_entry_id: from.get().checked_sub(1).map(id_from_number),
                        order: Some(ScanOrder::Descending),
                    },
                    1,
                    None,
                    context,
                )
                .await?
                .items
                .into_iter()
                .next();
            return Ok((entries, previous.map(|entry| entry.id)));
        }
    }
    let mut entries = scan_up_to(
        storage,
        EntryQuery {
            conversation_id: id,
            order: Some(ScanOrder::Descending),
            min_entry_id: None,
            max_entry_id: None,
        },
        min + 1,
        context,
    )
    .await?;
    let before = (entries.len() > min)
        .then(|| entries.pop().expect("entry past the window"))
        .map(|entry| entry.id);
    entries.reverse();
    Ok((entries, before))
}
