//! Storage reads gated by unpublished Session writes (port of
//! `packages/durable/src/session/keys.ts` @ `b4afdddf`).

use std::sync::{Arc, Weak};

use async_trait::async_trait;

use crate::chord::context::Context;
use crate::errors::{StorageError, StorageResult};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::session::SessionError;
use crate::session::keys::{
    ALL_KEYS, append_key, conversation_key, document_key, entry_key, incarnation_key, request_key,
    scope_key, submission_key, task_key,
};
use crate::session::session::SessionImpl;
use crate::types::{
    CommitResult, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentPoint,
    DocumentQuery, DocumentRecord, EntryQuery, EntryRecord, Page, Storage, StorageWrite,
    StoredDocument, StoredEntry, SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord,
};

pub(crate) struct GatedStorage {
    inner: Arc<dyn Storage>,
    session: Weak<SessionImpl>,
}

impl GatedStorage {
    pub(crate) fn new(inner: Arc<dyn Storage>, session: Weak<SessionImpl>) -> Arc<Self> {
        Arc::new(Self { inner, session })
    }

    async fn wait(&self, key: String) -> StorageResult<()> {
        let Some(session) = self.session.upgrade() else {
            return Err(StorageError::Closed("Session"));
        };
        session
            .wait_for_writer(&key)
            .await
            .map_err(storage_session_error)
    }

    async fn wait_entries(&self, conversation_id: ConversationId) -> StorageResult<()> {
        self.wait(conversation_key(conversation_id)).await?;
        self.wait(append_key(conversation_id)).await
    }
}

fn storage_session_error(error: SessionError) -> StorageError {
    match error {
        SessionError::Failed { cause } => StorageError::SessionFailed(cause),
        SessionError::Closed => StorageError::Closed("Session"),
        SessionError::Storage(error) => error,
        other => StorageError::message(other.to_string()),
    }
}

#[async_trait]
impl Storage for GatedStorage {
    async fn commit(
        &self,
        batches: &[Vec<StorageWrite>],
        receive: &mut (dyn FnMut(usize, Seq) + Send),
        context: &Context,
    ) -> StorageResult<CommitResult> {
        self.inner.commit(batches, receive, context).await
    }

    async fn mint_id(&self) -> StorageResult<u64> {
        self.inner.mint_id().await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageResult<Option<Arc<ConversationRecord>>> {
        self.wait(conversation_key(id)).await?;
        self.inner.conversation(id, context).await
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<ConversationRecord>> {
        self.wait(ALL_KEYS.to_string()).await?;
        self.inner
            .scan_conversations(query, limit, cursor, context)
            .await
    }

    async fn entry(&self, id: EntryId, context: &Context) -> StorageResult<Option<StoredEntry>> {
        self.wait(entry_key(id)).await?;
        self.inner.entry(id, context).await
    }

    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> StorageResult<Option<StoredEntry>> {
        self.wait(conversation_key(conversation_id)).await?;
        self.wait(entry_key(id)).await?;
        self.inner
            .entry_in_conversation(conversation_id, id, context)
            .await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        context: &Context,
    ) -> StorageResult<Option<EntryRecord>> {
        self.wait_entries(conversation_id).await?;
        self.inner
            .find_latest_head_marker(conversation_id, at_or_before_entry_id, context)
            .await
    }

    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<EntryRecord>> {
        self.wait_entries(query.conversation_id).await?;
        self.inner.scan_entries(query, limit, cursor, context).await
    }

    async fn task(&self, id: TaskId, context: &Context) -> StorageResult<Option<Arc<TaskRecord>>> {
        self.wait(task_key(id)).await?;
        self.inner.task(id, context).await
    }

    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<TaskRecord>> {
        self.wait(ALL_KEYS.to_string()).await?;
        self.inner.scan_tasks(query, limit, cursor, context).await
    }

    async fn submission(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> StorageResult<Option<Arc<SubmissionRecord>>> {
        self.wait(submission_key(id)).await?;
        self.inner.submission(id, context).await
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<SubmissionRecord>> {
        self.wait(ALL_KEYS.to_string()).await?;
        self.inner
            .scan_submissions(query, limit, cursor, context)
            .await
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> StorageResult<Option<Arc<SubmissionRecord>>> {
        self.wait(request_key(conversation_id, request_id)).await?;
        self.inner
            .submission_by_request(conversation_id, request_id, context)
            .await
    }

    async fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageResult<Option<DocumentRecord>> {
        self.wait(document_key(&crate::documents::address_id(&address)))
            .await?;
        self.inner.find_document(address, at, context).await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageResult<Option<StoredDocument>> {
        self.wait(incarnation_key(id)).await?;
        self.inner.document(id, at, context).await
    }

    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<DocumentRecord>> {
        self.wait(scope_key(&query.scope)).await?;
        self.inner
            .scan_documents(query, limit, cursor, context)
            .await
    }

    async fn close(&self, context: &Context) -> StorageResult<()> {
        self.inner.close(context).await
    }
}
