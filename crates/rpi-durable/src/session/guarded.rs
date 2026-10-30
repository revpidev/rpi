//! Storage failure guard (port of `GuardedStorage` in
//! `packages/durable/src/session/session.ts` @ `4a080252f`).
//!
//! Every Storage call a Session's components make goes through this wrapper.
//! The first error a Storage method throws — other than an invalid read
//! (`StorageRequestError`) or one whose caller was cancelled — fails the
//! Session. That call still gets the backend's error; every later call and
//! every Storage call still underway gets `SessionFailed`.

use std::future::Future;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::chord::context::Context;
use crate::errors::{StorageError, StorageResult};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::session::session::SessionImpl;
use crate::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentPoint, DocumentQuery,
    DocumentRecord, EntryQuery, EntryRecord, Page, Storage, StorageWrite, StoredDocument,
    StoredEntry, SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord,
};

#[derive(Default)]
struct Underway {
    closing: bool,
    calls: usize,
}

/// Storage wrapper that latches the first fatal error into its Session.
pub struct GuardedStorage {
    inner: Arc<dyn Storage>,
    session: Mutex<Weak<SessionImpl>>,
    underway: Mutex<Underway>,
    settled: Notify,
}

struct CallGuard<'a> {
    storage: &'a GuardedStorage,
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut underway) = self.storage.underway.lock() {
            underway.calls -= 1;
            if underway.calls == 0 {
                self.storage.settled.notify_waiters();
            }
        }
    }
}

impl GuardedStorage {
    /// Wrap `inner`; attach the owning Session once it exists.
    pub fn new(inner: Arc<dyn Storage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            session: Mutex::new(Weak::new()),
            underway: Mutex::new(Underway::default()),
            settled: Notify::new(),
        })
    }

    /// Attach the Session this wrapper guards.
    pub fn attach(&self, session: &Arc<SessionImpl>) {
        if let Ok(mut slot) = self.session.lock() {
            *slot = Arc::downgrade(session);
        }
    }

    fn fail(&self, error: &StorageError) {
        let session = self.session.lock().ok().and_then(|slot| slot.upgrade());
        if let Some(session) = session {
            session.fail(error.to_string());
        }
    }

    fn assert_healthy(&self) -> StorageResult<()> {
        let failure = self
            .session
            .lock()
            .ok()
            .and_then(|slot| slot.upgrade())
            .and_then(|session| session.failure());
        match failure {
            Some(cause) => Err(StorageError::SessionFailed(cause)),
            None => Ok(()),
        }
    }

    fn begin_call(&self) -> StorageResult<CallGuard<'_>> {
        let mut underway = self
            .underway
            .lock()
            .map_err(|_| StorageError::message("Storage guard is poisoned"))?;
        if underway.closing {
            return Err(StorageError::Closed("Session"));
        }
        underway.calls += 1;
        Ok(CallGuard { storage: self })
    }

    async fn call<T>(
        &self,
        read: Option<&Context>,
        run: impl Future<Output = StorageResult<T>> + Send,
    ) -> StorageResult<T> {
        self.assert_healthy()?;
        let _call = self.begin_call()?;
        let result = run.await;
        self.assert_healthy()?;
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Some(context) = read {
                    self.guard_read(&error, context);
                } else {
                    self.guard_write(&error);
                }
                Err(error)
            }
        }
    }

    /// A read error fails only its call when it is a request error or its
    /// caller was cancelled; any other read error fails the Session.
    fn guard_read(&self, error: &StorageError, context: &Context) {
        if error.is_request() {
            return;
        }
        if context
            .abort_signal()
            .is_some_and(|signal| signal.is_cancelled())
        {
            return;
        }
        self.fail(error);
    }

    fn guard_write(&self, error: &StorageError) {
        self.fail(error);
    }
}

#[async_trait]
impl Storage for GuardedStorage {
    async fn commit(&self, writes: &[StorageWrite], context: &Context) -> StorageResult<Seq> {
        self.call(None, self.inner.commit(writes, context)).await
    }

    async fn mint_id(&self) -> StorageResult<u64> {
        self.call(None, self.inner.mint_id()).await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageResult<Option<ConversationRecord>> {
        self.call(Some(context), self.inner.conversation(id, context))
            .await
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<ConversationRecord>> {
        self.call(
            Some(context),
            self.inner.scan_conversations(query, limit, cursor, context),
        )
        .await
    }

    async fn entry(&self, id: EntryId, context: &Context) -> StorageResult<Option<StoredEntry>> {
        self.call(Some(context), self.inner.entry(id, context))
            .await
    }

    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> StorageResult<Option<StoredEntry>> {
        self.call(
            Some(context),
            self.inner
                .entry_in_conversation(conversation_id, id, context),
        )
        .await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        context: &Context,
    ) -> StorageResult<Option<EntryRecord>> {
        self.call(
            Some(context),
            self.inner
                .find_latest_head_marker(conversation_id, at_or_before_entry_id, context),
        )
        .await
    }

    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<EntryRecord>> {
        self.call(
            Some(context),
            self.inner.scan_entries(query, limit, cursor, context),
        )
        .await
    }

    async fn task(&self, id: TaskId, context: &Context) -> StorageResult<Option<TaskRecord>> {
        self.call(Some(context), self.inner.task(id, context)).await
    }

    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<TaskRecord>> {
        self.call(
            Some(context),
            self.inner.scan_tasks(query, limit, cursor, context),
        )
        .await
    }

    async fn submission(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> StorageResult<Option<SubmissionRecord>> {
        self.call(Some(context), self.inner.submission(id, context))
            .await
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<SubmissionRecord>> {
        self.call(
            Some(context),
            self.inner.scan_submissions(query, limit, cursor, context),
        )
        .await
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> StorageResult<Option<SubmissionRecord>> {
        self.call(
            Some(context),
            self.inner
                .submission_by_request(conversation_id, request_id, context),
        )
        .await
    }

    async fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageResult<Option<DocumentRecord>> {
        self.call(
            Some(context),
            self.inner.find_document(address, at, context),
        )
        .await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageResult<Option<StoredDocument>> {
        self.call(Some(context), self.inner.document(id, at, context))
            .await
    }

    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<DocumentRecord>> {
        self.call(
            Some(context),
            self.inner.scan_documents(query, limit, cursor, context),
        )
        .await
    }

    async fn close(&self, context: &Context) -> StorageResult<()> {
        if let Ok(mut underway) = self.underway.lock() {
            underway.closing = true;
        }
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            // `notify_waiters` stores no permit: register before reading the
            // count so a call that finishes now cannot lose its wakeup.
            settled.as_mut().enable();
            let done = self
                .underway
                .lock()
                .map(|underway| underway.calls == 0)
                .unwrap_or(true);
            if done {
                break;
            }
            settled.await;
        }
        self.inner.close(context).await
    }
}
