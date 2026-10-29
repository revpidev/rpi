//! Storage failure guard (port of `GuardedStorage` in
//! `packages/durable/src/session/session.ts` @ `4a080252f`).
//!
//! Every Storage call a Session's components make goes through this wrapper.
//! The first error a Storage method throws — other than an invalid read
//! (`StorageRequestError`) or one whose caller was cancelled — fails the
//! Session. That call still gets the backend's error; every later call and
//! every Storage call still underway gets `SessionFailed`.

use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;

use crate::chord::context::Context;
use crate::errors::{StorageError, StorageResult};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::session::session::SessionImpl;
use crate::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentPoint, DocumentQuery,
    DocumentRecord, EntryQuery, EntryRecord, Page, Storage, StorageWrite, StoredDocument,
    StoredEntry, SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord,
};

/// Storage wrapper that latches the first fatal error into its Session.
pub struct GuardedStorage {
    inner: Arc<dyn Storage>,
    session: Mutex<Weak<SessionImpl>>,
}

impl GuardedStorage {
    /// Wrap `inner`; attach the owning Session once it exists.
    pub fn new(inner: Arc<dyn Storage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            session: Mutex::new(Weak::new()),
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
        match self.inner.commit(writes, context).await {
            Ok(seq) => Ok(seq),
            Err(error) => {
                self.guard_write(&error);
                Err(error)
            }
        }
    }

    async fn mint_id(&self) -> StorageResult<u64> {
        match self.inner.mint_id().await {
            Ok(id) => Ok(id),
            Err(error) => {
                self.guard_write(&error);
                Err(error)
            }
        }
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageResult<Option<ConversationRecord>> {
        match self.inner.conversation(id, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<ConversationRecord>> {
        match self
            .inner
            .scan_conversations(query, limit, cursor, context)
            .await
        {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn entry(&self, id: EntryId, context: &Context) -> StorageResult<Option<StoredEntry>> {
        match self.inner.entry(id, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> StorageResult<Option<StoredEntry>> {
        match self
            .inner
            .entry_in_conversation(conversation_id, id, context)
            .await
        {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        context: &Context,
    ) -> StorageResult<Option<EntryRecord>> {
        match self
            .inner
            .find_latest_head_marker(conversation_id, at_or_before_entry_id, context)
            .await
        {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<EntryRecord>> {
        match self.inner.scan_entries(query, limit, cursor, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn task(&self, id: TaskId, context: &Context) -> StorageResult<Option<TaskRecord>> {
        match self.inner.task(id, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<TaskRecord>> {
        match self.inner.scan_tasks(query, limit, cursor, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn submission(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> StorageResult<Option<SubmissionRecord>> {
        match self.inner.submission(id, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<SubmissionRecord>> {
        match self
            .inner
            .scan_submissions(query, limit, cursor, context)
            .await
        {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> StorageResult<Option<SubmissionRecord>> {
        match self
            .inner
            .submission_by_request(conversation_id, request_id, context)
            .await
        {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageResult<Option<DocumentRecord>> {
        match self.inner.find_document(address, at, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageResult<Option<StoredDocument>> {
        match self.inner.document(id, at, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<DocumentRecord>> {
        match self
            .inner
            .scan_documents(query, limit, cursor, context)
            .await
        {
            Ok(value) => Ok(value),
            Err(error) => {
                self.guard_read(&error, context);
                Err(error)
            }
        }
    }

    async fn close(&self, context: &Context) -> StorageResult<()> {
        self.inner.close(context).await
    }
}
