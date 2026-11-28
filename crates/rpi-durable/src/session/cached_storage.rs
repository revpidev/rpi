//! Publication-synchronous hot-record cache owned by one writable Session.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::chord::context::Context;
use crate::errors::StorageResult;
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::session::CommitChange;
use crate::types::{
    CommitResult, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentPoint,
    DocumentQuery, DocumentRecord, EntryQuery, EntryRecord, Page, Storage, StorageWrite,
    StoredDocument, StoredEntry, SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord,
    TaskState,
};

struct Lru<K, V> {
    capacity: usize,
    values: HashMap<K, Arc<V>>,
    order: VecDeque<K>,
}

impl<K: Eq + Hash + Clone, V> Lru<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            values: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &K) -> Option<Arc<V>> {
        let value = self.values.get(key).cloned();
        if value.is_some() {
            self.order.retain(|held| held != key);
            self.order.push_back(key.clone());
        }
        value
    }

    fn set(&mut self, key: K, value: Arc<V>) {
        self.values.insert(key.clone(), value);
        self.order.retain(|held| held != &key);
        self.order.push_back(key);
        while self.values.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.values.remove(&oldest);
        }
    }

    fn remove(&mut self, key: &K) -> Option<Arc<V>> {
        self.order.retain(|held| held != key);
        self.values.remove(key)
    }

    fn len(&self) -> usize {
        self.values.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReadKey {
    Conversation(ConversationId),
    Task(TaskId),
}

#[derive(Debug, Clone, Copy)]
struct ReadState {
    count: usize,
    version: u64,
}

struct Awaited {
    count: usize,
    record: Option<Arc<TaskRecord>>,
}

struct Wait {
    on: Vec<TaskId>,
    left: bool,
}

#[derive(Default)]
struct CacheState {
    conversations: Option<Lru<ConversationId, ConversationRecord>>,
    live_tasks: HashMap<TaskId, Arc<TaskRecord>>,
    tasks: Option<Lru<TaskId, TaskRecord>>,
    submissions: Option<Lru<SubmissionId, SubmissionRecord>>,
    awaited: HashMap<TaskId, Awaited>,
    waits: HashMap<TaskId, Wait>,
    reads: HashMap<ReadKey, ReadState>,
}

/// Immutable-record cache counts, separate from prepared statements and waiters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachedStorageSizes {
    pub conversations: usize,
    pub live_tasks: usize,
    pub tasks: usize,
    pub submissions: usize,
    pub awaited: usize,
    pub waits: usize,
    pub reads_in_flight: usize,
}

/// Hot Storage reads shared as immutable `Arc` values and updated at publication.
pub struct CachedStorage {
    storage: Arc<dyn Storage>,
    state: Mutex<CacheState>,
}

impl CachedStorage {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage,
            state: Mutex::new(CacheState {
                conversations: Some(Lru::new(4096)),
                tasks: Some(Lru::new(256)),
                submissions: Some(Lru::new(256)),
                ..CacheState::default()
            }),
        }
    }

    pub fn sizes(&self) -> CachedStorageSizes {
        let state = self.state.lock().expect("cache state");
        CachedStorageSizes {
            conversations: state.conversations.as_ref().map_or(0, Lru::len),
            live_tasks: state.live_tasks.len(),
            tasks: state.tasks.as_ref().map_or(0, Lru::len),
            submissions: state.submissions.as_ref().map_or(0, Lru::len),
            awaited: state.awaited.len(),
            waits: state.waits.len(),
            reads_in_flight: state.reads.len(),
        }
    }

    /// Apply one adopted and stored publication; never call at the Storage receipt.
    pub fn published(&self, changes: &[CommitChange]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        for change in changes {
            let CommitChange::Table(write) = change else {
                continue;
            };
            match write {
                StorageWrite::Conversation { value } => {
                    wrote(&mut state, ReadKey::Conversation(value.id));
                    state
                        .conversations
                        .as_mut()
                        .expect("conversation cache")
                        .set(value.id, Arc::new(value.clone()));
                }
                StorageWrite::Task { value } => {
                    wrote(&mut state, ReadKey::Task(value.id));
                    follow_wait(&mut state, value);
                    keep_task(&mut state, Arc::new(value.clone()));
                }
                StorageWrite::Submission { value } => {
                    state
                        .submissions
                        .as_mut()
                        .expect("submission cache")
                        .set(value.id(), Arc::new(value.clone()));
                }
                _ => {}
            }
        }
    }

    fn begin_read(&self, key: ReadKey) -> u64 {
        let mut state = self.state.lock().expect("cache state");
        let read = state.reads.entry(key).or_insert(ReadState {
            count: 0,
            version: 0,
        });
        read.count += 1;
        read.version
    }

    fn finish_read<F>(&self, key: ReadKey, version: u64, keep: Option<F>)
    where
        F: FnOnce(&mut CacheState),
    {
        let mut state = self.state.lock().expect("cache state");
        if state
            .reads
            .get(&key)
            .is_some_and(|read| read.version == version)
            && let Some(keep) = keep
        {
            keep(&mut state);
        }
        if let Some(read) = state.reads.get_mut(&key) {
            read.count = read.count.saturating_sub(1);
            if read.count == 0 {
                state.reads.remove(&key);
            }
        }
    }
}

fn wrote(state: &mut CacheState, key: ReadKey) {
    if let Some(read) = state.reads.get_mut(&key) {
        read.version = read.version.wrapping_add(1);
    }
}

fn keep_task(state: &mut CacheState, record: Arc<TaskRecord>) {
    if matches!(record.state, TaskState::Terminal { .. }) {
        state.live_tasks.remove(&record.id);
        if let Some(awaited) = state.awaited.get_mut(&record.id) {
            awaited.record = Some(record);
        } else {
            state
                .tasks
                .as_mut()
                .expect("task cache")
                .set(record.id, record);
        }
    } else {
        state.tasks.as_mut().expect("task cache").remove(&record.id);
        state.live_tasks.insert(record.id, record);
    }
}

fn follow_wait(state: &mut CacheState, record: &TaskRecord) {
    if let TaskState::Waiting { on, .. } = &record.state {
        if let Some(previous) = state.waits.remove(&record.id) {
            release(state, &previous.on);
        }
        let on = on.clone();
        state.waits.insert(
            record.id,
            Wait {
                on: on.clone(),
                left: false,
            },
        );
        for id in on {
            let ended = state.tasks.as_mut().expect("task cache").remove(&id);
            let awaited = state.awaited.entry(id).or_insert(Awaited {
                count: 0,
                record: ended,
            });
            awaited.count += 1;
        }
        return;
    }
    let Some(previous) = state.waits.get_mut(&record.id) else {
        return;
    };
    if !previous.left && !matches!(record.state, TaskState::Terminal { .. }) {
        previous.left = true;
        return;
    }
    let previous = state.waits.remove(&record.id).expect("wait");
    release(state, &previous.on);
}

fn release(state: &mut CacheState, on: &[TaskId]) {
    for id in on {
        let Some(awaited) = state.awaited.get_mut(id) else {
            continue;
        };
        awaited.count = awaited.count.saturating_sub(1);
        if awaited.count > 0 {
            continue;
        }
        let awaited = state.awaited.remove(id).expect("awaited");
        if let Some(record) = awaited.record {
            state.tasks.as_mut().expect("task cache").set(*id, record);
        }
    }
}

#[async_trait]
impl Storage for CachedStorage {
    async fn commit(
        &self,
        batches: &[Vec<StorageWrite>],
        receive: &mut (dyn FnMut(usize, Seq) + Send),
        context: &Context,
    ) -> StorageResult<CommitResult> {
        self.storage.commit(batches, receive, context).await
    }

    async fn mint_id(&self) -> StorageResult<u64> {
        self.storage.mint_id().await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageResult<Option<Arc<ConversationRecord>>> {
        if let Some(value) = self
            .state
            .lock()
            .expect("cache state")
            .conversations
            .as_mut()
            .expect("conversation cache")
            .get(&id)
        {
            return Ok(Some(value));
        }
        let key = ReadKey::Conversation(id);
        let version = self.begin_read(key);
        let result = self.storage.conversation(id, context).await;
        match &result {
            Ok(Some(value)) => {
                let value = Arc::clone(value);
                self.finish_read(
                    key,
                    version,
                    Some(move |state: &mut CacheState| {
                        state
                            .conversations
                            .as_mut()
                            .expect("conversation cache")
                            .set(id, value);
                    }),
                );
            }
            _ => self.finish_read(key, version, None::<fn(&mut CacheState)>),
        }
        result
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<ConversationRecord>> {
        self.storage
            .scan_conversations(query, limit, cursor, context)
            .await
    }

    async fn entry(&self, id: EntryId, context: &Context) -> StorageResult<Option<StoredEntry>> {
        self.storage.entry(id, context).await
    }

    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> StorageResult<Option<StoredEntry>> {
        self.storage
            .entry_in_conversation(conversation_id, id, context)
            .await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        context: &Context,
    ) -> StorageResult<Option<EntryRecord>> {
        self.storage
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
        self.storage
            .scan_entries(query, limit, cursor, context)
            .await
    }

    async fn task(&self, id: TaskId, context: &Context) -> StorageResult<Option<Arc<TaskRecord>>> {
        let cached = {
            let mut state = self.state.lock().expect("cache state");
            state.live_tasks.get(&id).cloned().or_else(|| {
                state
                    .awaited
                    .get(&id)
                    .and_then(|awaited| awaited.record.clone())
                    .or_else(|| state.tasks.as_mut().expect("task cache").get(&id))
            })
        };
        if let Some(value) = cached {
            return Ok(Some(value));
        }
        let key = ReadKey::Task(id);
        let version = self.begin_read(key);
        let result = self.storage.task(id, context).await;
        match &result {
            Ok(Some(value)) => {
                let value = Arc::clone(value);
                self.finish_read(
                    key,
                    version,
                    Some(move |state: &mut CacheState| {
                        keep_task(state, value);
                    }),
                );
            }
            _ => self.finish_read(key, version, None::<fn(&mut CacheState)>),
        }
        result
    }

    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<TaskRecord>> {
        self.storage.scan_tasks(query, limit, cursor, context).await
    }

    async fn submission(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> StorageResult<Option<Arc<SubmissionRecord>>> {
        let cached = self
            .state
            .lock()
            .expect("cache state")
            .submissions
            .as_mut()
            .expect("submission cache")
            .get(&id);
        match cached {
            Some(value) => Ok(Some(value)),
            None => self.storage.submission(id, context).await,
        }
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<SubmissionRecord>> {
        self.storage
            .scan_submissions(query, limit, cursor, context)
            .await
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> StorageResult<Option<Arc<SubmissionRecord>>> {
        self.storage
            .submission_by_request(conversation_id, request_id, context)
            .await
    }

    async fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageResult<Option<DocumentRecord>> {
        self.storage.find_document(address, at, context).await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageResult<Option<StoredDocument>> {
        self.storage.document(id, at, context).await
    }

    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageResult<Page<DocumentRecord>> {
        self.storage
            .scan_documents(query, limit, cursor, context)
            .await
    }

    async fn close(&self, context: &Context) -> StorageResult<()> {
        self.storage.close(context).await
    }
}
