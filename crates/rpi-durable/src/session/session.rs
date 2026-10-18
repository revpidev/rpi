//! Session kernel: one mutation line, the loaded document tracker cache, and
//! committed publication (port of `packages/durable/src/session/session.ts`
//! @ `abe508e1b`).
//!
//! Only committed state is observable. Every commit callback, storage
//! settlement, adoption and publication runs while the single mutation line is
//! held; listeners run after adoption but before the line releases, and must not
//! re-enter.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::chord::context::{AbortSignal, Context, without_abort_signal};
use crate::chord::json::{JsonObject, JsonValue};
use crate::chord::tracker::track;
use crate::documents::{
    DocScope, DocTarget, DocToken, check_record_scope_record, check_record_version_record,
    materialize_document, resolve_address,
};
use crate::errors::StorageError;
use crate::ids::{ConversationId, DocumentId, EntryId, Seq};
use crate::session::observation::{
    CommittedStateSource, CommittedWatch, WatchListener, attach_state,
};
use crate::session::transaction::{LoadedDocument, Transaction, TransactionScope};
use crate::session::{
    CommitChange, CommitPublication, DocOwner, SessionError, SessionFuture, SessionResult,
    Subscription, doc_target, observed_operations,
};
use crate::types::{ConversationRecord, DocumentAddress, DocumentRecord, DocumentScope, Storage};

/// Wall clock used for task lifecycle times (upstream `Date.now`).
pub type NowFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// A synchronous post-adoption commit listener. It must not throw, block, or
/// call session operations (upstream `subscribeCommits`).
pub type CommitListener = Arc<dyn Fn(&CommitPublication, &Context) + Send + Sync>;

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
/// cancelled or failed await leaves the resource retryable.
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
    storage: Arc<dyn Storage>,
    now: NowFn,
    documents: Mutex<HashMap<String, Arc<LoadedDocument>>>,
    commit_listeners: Arc<Mutex<Vec<(u64, CommitListener)>>>,
    close_listeners: Arc<Mutex<Vec<(u64, CloseListener)>>>,
    conversation_created_hook: Mutex<Option<ConversationCreatedHook>>,
    line: tokio::sync::Mutex<()>,
    closing: AtomicBool,
    poisoned: AtomicBool,
    cleanup_done: AtomicBool,
}

/// Open a session kernel over one storage backend (upstream `createSession`).
pub fn create_session(storage: Arc<dyn Storage>, now: Option<NowFn>) -> Arc<SessionImpl> {
    SessionImpl::new(storage, now)
}

impl SessionImpl {
    /// Build a session over `storage` with the given clock (default [`crate::session::default_now`]).
    pub fn new(storage: Arc<dyn Storage>, now: Option<NowFn>) -> Arc<Self> {
        Arc::new(Self {
            storage,
            now: now.unwrap_or_else(|| Arc::new(crate::session::default_now)),
            documents: Mutex::new(HashMap::new()),
            commit_listeners: Arc::new(Mutex::new(Vec::new())),
            close_listeners: Arc::new(Mutex::new(Vec::new())),
            conversation_created_hook: Mutex::new(None),
            line: tokio::sync::Mutex::new(()),
            closing: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            cleanup_done: AtomicBool::new(false),
        })
    }

    /// The storage backend.
    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.storage
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

    pub(crate) fn cached(&self, address_id: &str) -> Option<Arc<LoadedDocument>> {
        self.documents
            .lock()
            .ok()
            .and_then(|documents| documents.get(address_id).cloned())
    }

    pub(crate) fn install(&self, document: Arc<LoadedDocument>) {
        if let Ok(mut documents) = self.documents.lock() {
            documents.insert(document.address_id.clone(), document);
        }
    }

    pub(crate) fn evict(&self, address_id: &str, record_id: DocumentId) {
        if let Ok(mut documents) = self.documents.lock()
            && documents
                .get(address_id)
                .map(|document| document.record.id == record_id)
                .unwrap_or(false)
        {
            documents.remove(address_id);
        }
    }

    pub(crate) async fn load_document(
        &self,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: &str,
        address: &DocumentAddress,
        context: &Context,
    ) -> SessionResult<Option<Arc<LoadedDocument>>> {
        let cached = self.cached(address_id);
        if let Some(cached) = &cached
            && cached.value_version == definition.version
        {
            return Ok(Some(Arc::clone(cached)));
        }
        if cached.is_some()
            && let Ok(mut documents) = self.documents.lock()
        {
            documents.remove(address_id);
        }
        let record = self
            .storage
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
            .storage
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
        Box::pin(async move { this.run_commit(change, context, scope).await })
    }

    /// Commit one callback (upstream `commit`).
    pub fn commit<T: Send + 'static>(
        self: &Arc<Self>,
        change: CommitFn<T>,
        context: &Context,
    ) -> SessionFuture<'_, T> {
        self.commit_with(change, context, None)
    }

    async fn run_commit<T: Send>(
        self: Arc<Self>,
        change: CommitFn<T>,
        context: Context,
        scope: Option<TransactionScope>,
    ) -> SessionResult<T> {
        let _guard = self.line.lock().await;
        self.assert_healthy()?;
        if context
            .abort_signal()
            .is_some_and(|signal| signal.is_cancelled())
        {
            return Err(SessionError::Aborted);
        }
        let mut tx = Transaction::new(
            Arc::clone(&self),
            context.clone(),
            scope.unwrap_or_default(),
        );
        let result = match (change)(&mut tx).await {
            Ok(value) => value,
            Err(error) => {
                tx.settle_failure().await;
                return Err(error);
            }
        };
        let writes = tx.settle_success().await?;
        if writes.is_empty() {
            tx.discard();
            return Ok(result);
        }
        let seq = match self
            .storage
            .commit(&writes, &without_abort_signal(&context))
            .await
        {
            Ok(seq) => seq,
            Err(error) => {
                tx.discard();
                if !matches!(error, StorageError::Rejected(_)) {
                    self.poisoned.store(true, Ordering::SeqCst);
                }
                return Err(SessionError::Storage(error));
            }
        };
        let documents = match tx.adopt(seq) {
            Ok(documents) => documents,
            Err(error) => {
                self.poisoned.store(true, Ordering::SeqCst);
                return Err(error);
            }
        };
        self.publish(seq, &writes, &documents, &context);
        Ok(result)
    }

    fn publish(
        &self,
        seq: Seq,
        writes: &[crate::types::StorageWrite],
        documents: &[CommitChange],
        context: &Context,
    ) {
        let listeners: Vec<CommitListener> = match self.commit_listeners.lock() {
            Ok(listeners) => listeners
                .iter()
                .map(|(_, listener)| Arc::clone(listener))
                .collect(),
            Err(_) => return,
        };
        if listeners.is_empty() {
            return;
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
        for listener in listeners {
            listener(&publication, context);
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
            let _guard = this.line.lock().await;
            this.assert_healthy()?;
            job().await
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
        &self,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: String,
        address: DocumentAddress,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        if let Some(cached) = self.cached(&address_id)
            && cached.value_version == definition.version
        {
            check_record_scope_record(definition, &cached.record)?;
            check_record_version_record(definition, &cached.record, cached.stored_version())?;
            return Ok(cached.tracker.value().as_object().cloned());
        }
        let _guard = self.line.lock().await;
        self.assert_healthy()?;
        let loaded = self
            .load_document(definition, &address_id, &address, context)
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
        &self,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: String,
        address: DocumentAddress,
        context: &Context,
    ) -> SessionResult<Option<crate::chord::state::AttachedReplicatedState>> {
        let _guard = self.line.lock().await;
        self.assert_healthy()?;
        let loaded = self
            .load_document(definition, &address_id, &address, context)
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
        let subscription_commit = self.subscribe_commits(Arc::new(move |publication, context| {
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
        let subscription_close =
            self.subscribe_close(Arc::new(move || source_close.close_session()));
        source.set_release(Box::new(move || {
            subscription_commit.unsubscribe();
            subscription_close.unsubscribe();
        }));
        Ok(Some(attach_state(&source)))
    }

    /// Attach a serialized async watch to one document (upstream `watchDoc`).
    pub fn watch_doc(
        self: &Arc<Self>,
        token: &DocToken,
        owner: DocOwner<'_>,
        context: &Context,
    ) -> SessionFuture<'_, Option<CommittedWatch>> {
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
            this.watch_doc_resolved(&definition, resolved.id, resolved.address, &context, signal)
                .await
        })
    }

    async fn watch_doc_resolved(
        &self,
        definition: &Arc<crate::documents::DocDefinition>,
        address_id: String,
        address: DocumentAddress,
        context: &Context,
        signal: Option<AbortSignal>,
    ) -> SessionResult<Option<CommittedWatch>> {
        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return Err(SessionError::Aborted);
        }
        let _guard = self.line.lock().await;
        self.assert_healthy()?;
        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return Err(SessionError::Aborted);
        }
        let loaded = self
            .load_document(definition, &address_id, &address, context)
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
        let record_id = loaded.record.id;
        let watch_commit = watch.clone();
        let subscription_commit = self.subscribe_commits(Arc::new(move |publication, context| {
            for change in &publication.changes {
                if let CommitChange::Document(document) = change {
                    if document.record.id != record_id {
                        continue;
                    }
                    let ops = observed_operations(&observed, document);
                    if ops.is_empty() {
                        continue;
                    }
                    watch_commit.advance(document.frame_value(), ops, context.clone());
                }
            }
        }));
        let watch_close = watch.clone();
        let subscription_close =
            self.subscribe_close(Arc::new(move || watch_close.close_session()));
        watch.set_detach(Box::new(move || {
            subscription_commit.unsubscribe();
            subscription_close.unsubscribe();
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
        &self,
        definition: &Arc<crate::documents::DocDefinition>,
        conversation: ConversationId,
        at: EntryId,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        let _guard = self.line.lock().await;
        self.assert_healthy()?;
        let stored_entry = self
            .storage
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
        let record = self.storage.find_document(address, point, context).await?;
        let Some(record) = record else {
            return Ok(None);
        };
        let stored = self
            .storage
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
        &self,
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
            if let Ok(mut documents) = this.documents.lock() {
                documents.clear();
            }
            Ok(())
        })
    }

    // ─── Close ──────────────────────────────────────────────────────────────

    /// Seal admission and close storage (upstream `close`).
    pub fn close(self: &Arc<Self>, context: &Context) -> SessionFuture<'_, ()> {
        if !self.closing.swap(true, Ordering::SeqCst) {
            let listeners: Vec<CloseListener> = match self.close_listeners.lock() {
                Ok(mut listeners) => listeners.drain(..).map(|(_, listener)| listener).collect(),
                Err(_) => Vec::new(),
            };
            for listener in listeners {
                listener();
            }
        }
        let this = Arc::clone(self);
        let context = without_abort_signal(context);
        Box::pin(async move {
            let _guard = this.line.lock().await;
            if this.cleanup_done.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            // If closing is cancelled or fails, the flag must go back to
            // false so a later close can retry (the old code left the
            // session permanently "cleaned up" without a closed storage).
            let mut reset = ResetOnDrop::new(&this.cleanup_done);
            if let Ok(mut documents) = this.documents.lock() {
                documents.clear();
            }
            if let Ok(mut listeners) = this.commit_listeners.lock() {
                listeners.clear();
            }
            this.storage.close(&context).await?;
            reset.disarm();
            Ok(())
        })
    }

    // ─── Listeners ──────────────────────────────────────────────────────────

    /// Register a synchronous post-adoption commit listener (upstream `subscribeCommits`).
    pub fn subscribe_commits(&self, listener: CommitListener) -> Subscription {
        let store = Arc::clone(&self.commit_listeners);
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut listeners) = store.lock() {
            listeners.push((id, listener));
        }
        Subscription::new(move || {
            if let Ok(mut listeners) = store.lock() {
                listeners.retain(|(existing, _)| *existing != id);
            }
        })
    }

    /// Register a synchronous close listener (upstream `subscribeClose`).
    pub fn subscribe_close(&self, listener: CloseListener) -> Subscription {
        let store = Arc::clone(&self.close_listeners);
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut listeners) = store.lock() {
            listeners.push((id, listener));
        }
        Subscription::new(move || {
            if let Ok(mut listeners) = store.lock() {
                listeners.retain(|(existing, _)| *existing != id);
            }
        })
    }

    fn assert_usable(&self) -> SessionResult<()> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(SessionError::Closed);
        }
        self.assert_healthy()
    }

    fn assert_healthy(&self) -> SessionResult<()> {
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(SessionError::Poisoned);
        }
        Ok(())
    }
}

/// Install a watch listener helper used by tests and consumers.
pub fn watch_listener<F>(function: F) -> WatchListener
where
    F: Fn(
            crate::session::ObservedDocumentValue,
            Vec<crate::chord::delta::Op>,
            Context,
        ) -> crate::session::WatchFuture
        + Send
        + Sync
        + 'static,
{
    Arc::new(function)
}
