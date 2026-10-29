//! Session kernel: one mutation line, the loaded document tracker cache, and
//! committed publication (port of `packages/durable/src/session/session.ts`
//! @ `4a080252f`).
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
use crate::ids::{ConversationId, DocumentId, EntryId, Seq};
use crate::session::guarded::GuardedStorage;
use crate::session::observation::{
    CommittedStateSource, CommittedWatch, WatchListener, attach_state,
};
use crate::session::transaction::{LoadedDocument, Transaction, TransactionScope};
use crate::session::{
    CommitChange, CommitPublication, DocOwner, SessionEnd, SessionError, SessionFuture,
    SessionResult, Subscription, doc_target, observed_operations,
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
    raw: Arc<dyn Storage>,
    guarded: Arc<dyn Storage>,
    guarded_typed: Arc<GuardedStorage>,
    now: NowFn,
    documents: Mutex<HashMap<String, Arc<LoadedDocument>>>,
    /// Commit listeners: `true` marks the Session's own component listener,
    /// whose throw fails the Session; host listeners report only.
    commit_listeners: Arc<Mutex<Vec<(u64, CommitListener, bool)>>>,
    close_listeners: Arc<Mutex<Vec<(u64, CloseListener)>>>,
    conversation_created_hook: Mutex<Option<ConversationCreatedHook>>,
    line: tokio::sync::Mutex<()>,
    closing: AtomicBool,
    cleanup_done: AtomicBool,
    /// The first error that failed the Session.
    failure: Mutex<Option<String>>,
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
    /// Addresses with a watch or state attached; never evicted.
    attached: Mutex<std::collections::HashSet<String>>,
    /// `settings.documentRetentionMs`, read at each use.
    document_retention_ms: Mutex<Option<DocumentRetentionReader>>,
}

/// Reads `settings.documentRetentionMs` at each use.
pub type DocumentRetentionReader = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// Host reporter for the error that failed the Session.
pub type FailureReporter = Arc<dyn Fn(String) + Send + Sync>;
/// Callback run when the Session fails.
pub type FailureHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Open a session kernel over one storage backend (upstream `createSession`).
pub fn create_session(storage: Arc<dyn Storage>, now: Option<NowFn>) -> Arc<SessionImpl> {
    SessionImpl::new(storage, now)
}

impl SessionImpl {
    /// Build a session over `storage` with the given clock (default [`crate::session::default_now`]).
    pub fn new(storage: Arc<dyn Storage>, now: Option<NowFn>) -> Arc<Self> {
        let guarded_typed = GuardedStorage::new(Arc::clone(&storage));
        let guarded: Arc<dyn Storage> = guarded_typed.clone();
        let session = Arc::new(Self {
            raw: Arc::clone(&storage),
            guarded,
            guarded_typed,
            now: now.unwrap_or_else(|| Arc::new(crate::session::default_now)),
            documents: Mutex::new(HashMap::new()),
            commit_listeners: Arc::new(Mutex::new(Vec::new())),
            close_listeners: Arc::new(Mutex::new(Vec::new())),
            conversation_created_hook: Mutex::new(None),
            line: tokio::sync::Mutex::new(()),
            closing: AtomicBool::new(false),
            cleanup_done: AtomicBool::new(false),
            failure: Mutex::new(None),
            end: Mutex::new(None),
            end_notify: tokio::sync::Notify::new(),
            failure_reporter: Mutex::new(None),
            listener_reporter: Mutex::new(None),
            failure_hooks: Mutex::new(Vec::new()),
            idle_since: Mutex::new(HashMap::new()),
            attached: Mutex::new(std::collections::HashSet::new()),
            document_retention_ms: Mutex::new(None),
        });
        session.guarded_typed.attach(&session);
        session
    }

    /// The storage backend, guarded so a failure fails this Session.
    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.guarded
    }

    /// The raw storage backend, for closing and tests.
    pub fn raw_storage(&self) -> &Arc<dyn Storage> {
        &self.raw
    }

    /// The first error that failed the Session, if one did.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
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

    /// Fail the Session: the first failure wins. Admission ends, the close
    /// listeners run at once, and Storage closes in the background. Later
    /// calls and waits reject with `SessionFailed`.
    pub fn fail(self: &Arc<Self>, cause: impl Into<String>) {
        let cause = cause.into();
        {
            let Ok(mut failure) = self.failure.lock() else {
                return;
            };
            if failure.is_some() {
                return;
            }
            *failure = Some(cause.clone());
        }
        self.closing.store(true, Ordering::SeqCst);
        self.set_end(SessionEnd::Failed {
            error: cause.clone(),
        });
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
            reporter(cause.clone());
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let this = Arc::clone(self);
            handle.spawn(async move {
                let _ = this.close(&crate::chord::context::BACKGROUND_CONTEXT).await;
            });
        }
    }

    /// Resolve with the terminal session state once close or failure reaches it.
    pub async fn closed(&self) -> SessionEnd {
        loop {
            let notified = self.end_notify.notified();
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
                reporter("A close listener panicked".to_string());
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

    pub(crate) fn cached(&self, address_id: &str) -> Option<Arc<LoadedDocument>> {
        self.evict_idle();
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

    /// Install the live `documentRetentionMs` reader.
    pub fn set_document_retention(&self, retention: DocumentRetentionReader) {
        if let Ok(mut slot) = self.document_retention_ms.lock() {
            *slot = Some(retention);
        }
    }

    fn touch(&self, address_id: &str) {
        if let Ok(mut idle) = self.idle_since.lock() {
            idle.insert(address_id.to_string(), (self.now)());
        }
    }

    /// Drop documents with no watch or state attached once they were not used
    /// for `documentRetentionMs` (default ten minutes; `0` drops immediately,
    /// `Infinity` keeps).
    fn evict_idle(&self) {
        let retention_reader = self
            .document_retention_ms
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        let retention = match retention_reader.as_ref().and_then(|reader| reader()) {
            Some(ms) => ms as f64,
            None => 600_000.0,
        };
        let now = (self.now)();
        let attached = self
            .attached
            .lock()
            .ok()
            .map(|set| set.clone())
            .unwrap_or_default();
        let idle = match self.idle_since.lock() {
            Ok(idle) => idle,
            Err(_) => return,
        };
        let expired: Vec<String> = idle
            .iter()
            .filter(|(address, since)| !attached.contains(*address) && now - **since >= retention)
            .map(|(address, _)| address.clone())
            .collect();
        drop(idle);
        if expired.is_empty() {
            return;
        }
        if let Ok(mut documents) = self.documents.lock() {
            for address in &expired {
                documents.remove(address);
            }
        }
        if let Ok(mut idle) = self.idle_since.lock() {
            for address in expired {
                idle.remove(&address);
            }
        }
    }

    pub(crate) fn attach_document(&self, address_id: &str) {
        if let Ok(mut attached) = self.attached.lock() {
            attached.insert(address_id.to_string());
        }
    }

    pub(crate) fn detach_document(&self, address_id: &str) {
        if let Ok(mut attached) = self.attached.lock() {
            attached.remove(address_id);
        }
        self.touch(address_id);
    }

    pub(crate) fn install(&self, document: Arc<LoadedDocument>) {
        let address = document.address_id.clone();
        if let Ok(mut documents) = self.documents.lock() {
            documents.insert(address.clone(), document);
        }
        self.touch(&address);
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
            .guarded
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
            .guarded
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
            .storage()
            .commit(&writes, &without_abort_signal(&context))
            .await
        {
            Ok(seq) => seq,
            Err(error) => {
                tx.discard();
                self.fail(error.to_string());
                return Err(SessionError::Storage(error));
            }
        };
        let documents = match tx.adopt(seq) {
            Ok(documents) => documents,
            Err(error) => {
                self.fail(error.to_string());
                return Err(error);
            }
        };
        self.publish(seq, &writes, &documents, &context);
        Ok(result)
    }

    fn publish(
        self: &Arc<Self>,
        seq: Seq,
        writes: &[crate::types::StorageWrite],
        documents: &[CommitChange],
        context: &Context,
    ) {
        let listeners: Vec<(CommitListener, bool)> = match self.commit_listeners.lock() {
            Ok(listeners) => listeners
                .iter()
                .map(|(_, listener, internal)| (Arc::clone(listener), *internal))
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
        for (listener, internal) in listeners {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                listener(&publication, context);
            }));
            if let Err(payload) = outcome {
                let message = panic_message(payload);
                if internal {
                    self.fail(format!("A component commit listener failed: {message}"));
                } else if let Ok(reporter) = self.listener_reporter.lock()
                    && let Some(reporter) = reporter.as_ref()
                {
                    reporter(format!("A commit listener failed: {message}"));
                }
            }
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
        self: &Arc<Self>,
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
        let weak = Arc::downgrade(self);
        let subscription_close = self.subscribe_close(Arc::new(move || {
            let failure = weak.upgrade().and_then(|session| session.failure());
            source_close.close_session(failure);
        }));
        self.attach_document(&address_id);
        let weak = Arc::downgrade(self);
        let attached = address_id.clone();
        source.set_release(Box::new(move || {
            subscription_commit.unsubscribe();
            subscription_close.unsubscribe();
            if let Some(session) = weak.upgrade() {
                session.detach_document(&attached);
            }
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
        self: &Arc<Self>,
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
        let weak = Arc::downgrade(self);
        let subscription_close = self.subscribe_close(Arc::new(move || {
            let failure = weak.upgrade().and_then(|session| session.failure());
            watch_close.close_session(failure);
        }));
        self.attach_document(&address_id);
        let weak = Arc::downgrade(self);
        let attached = address_id.clone();
        watch.set_detach(Box::new(move || {
            subscription_commit.unsubscribe();
            subscription_close.unsubscribe();
            if let Some(session) = weak.upgrade() {
                session.detach_document(&attached);
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
        &self,
        definition: &Arc<crate::documents::DocDefinition>,
        conversation: ConversationId,
        at: EntryId,
        context: &Context,
    ) -> SessionResult<Option<JsonObject>> {
        let _guard = self.line.lock().await;
        self.assert_healthy()?;
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
        let record = self.guarded.find_document(address, point, context).await?;
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
            self.notify_close();
        }
        let this = Arc::clone(self);
        let context = without_abort_signal(context);
        Box::pin(async move {
            let _guard = this.line.lock().await;
            if this.cleanup_done.swap(true, Ordering::SeqCst) {
                if this.failure().is_none() {
                    this.set_end(SessionEnd::Closed);
                }
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
            let result = this.raw.close(&context).await;
            if let Err(error) = &result {
                this.fail(error.to_string());
            }
            if this.failure().is_none() {
                this.set_end(SessionEnd::Closed);
            }
            reset.disarm();
            result?;
            Ok(())
        })
    }

    // ─── Listeners ──────────────────────────────────────────────────────────

    /// Register a synchronous post-adoption commit listener (upstream `subscribeCommits`).
    pub fn subscribe_commits(&self, listener: CommitListener) -> Subscription {
        let store = Arc::clone(&self.commit_listeners);
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut listeners) = store.lock() {
            listeners.push((id, listener, false));
        }
        Subscription::new(move || {
            if let Ok(mut listeners) = store.lock() {
                listeners.retain(|(existing, _, _)| *existing != id);
            }
        })
    }

    /// Register the Session's own component commit listener: its throw fails
    /// the Session (upstream `subscribeInternalCommits`).
    pub fn subscribe_internal_commits(&self, listener: CommitListener) -> Subscription {
        let store = Arc::clone(&self.commit_listeners);
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut listeners) = store.lock() {
            listeners.push((id, listener, true));
        }
        Subscription::new(move || {
            if let Ok(mut listeners) = store.lock() {
                listeners.retain(|(existing, _, _)| *existing != id);
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

/// The message of a caught panic payload.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
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
