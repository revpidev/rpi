//! Portable SQLite implementation of the storage contract
//! (port of `src/storage/sqlite/storage.ts` @ `b4afdddf`).

use async_trait::async_trait;
use std::sync::{Arc, Mutex};

use crate::chord::context::Context;
use crate::chord::delta::{Op, apply_immutable_batches};
use crate::chord::json::{JsonObject, JsonValue};
use crate::chord::sqlite::database::{SqliteDatabase, SqliteTransaction, SqliteValue};
use crate::errors::{StorageError, StorageResult};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::storage::scan::{ScanStart, scan_start};
use crate::storage::sqlite::commit_state::{CommitRefusal, CommitState, CopyResolution};
use crate::storage::sqlite::migrations::{
    apply_sqlite_migrations_for_schema, create_sqlite_migrations,
};
use crate::storage::sqlite::pending_writes::PendingWrites;
use crate::storage::sqlite::schema::{SqliteSchema, create_sqlite_schema};
use crate::storage::{is_alive_at, is_current_only, page_values};
use crate::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent,
    DocumentCreate, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope, EntryQuery,
    EntryRecord, Page, ScanOrder, Storage, StorageWrite, StoredDocument, StoredEntry,
    SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(super) fn encode_json<T: serde::Serialize>(value: &T) -> StorageResult<String> {
    Ok(serde_json::to_string(value)?)
}

/// Some SQLite bindings replace lone UTF-16 surrogates. JSON encoding keeps
/// indexed identities lossless.
pub(super) fn encode_indexed_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

struct ScopeColumns {
    scope_kind: &'static str,
    owner_id: i64,
}

fn scope_columns(scope: &DocumentScope) -> ScopeColumns {
    match scope {
        DocumentScope::Session => ScopeColumns {
            scope_kind: "session",
            owner_id: 0,
        },
        DocumentScope::Conversation {
            conversation_id, ..
        } => ScopeColumns {
            scope_kind: "conversation",
            owner_id: conversation_id.get() as i64,
        },
        DocumentScope::Task { task_id } => ScopeColumns {
            scope_kind: "task",
            owner_id: task_id.get() as i64,
        },
    }
}

pub(super) struct AddressParts {
    pub(super) kind: String,
    pub(super) scope_kind: &'static str,
    pub(super) owner_id: i64,
    pub(super) family: i64,
    pub(super) key_value: String,
}

pub(super) fn address_parts(kind: &str, scope: &DocumentScope, key: Option<&str>) -> AddressParts {
    let scope = scope_columns(scope);
    AddressParts {
        kind: encode_indexed_string(kind),
        scope_kind: scope.scope_kind,
        owner_id: scope.owner_id,
        family: if key.is_some() { 1 } else { 0 },
        key_value: encode_indexed_string(key.unwrap_or("")),
    }
}

fn address_parts_of(address: &DocumentAddress) -> AddressParts {
    address_parts(&address.kind, &address.scope, address.key.as_deref())
}

fn scan_sql(start: ScanStart) -> (&'static str, i64, &'static str) {
    match start.order {
        ScanOrder::Ascending => (
            "id > ?",
            start.after.map(|after| after as i64).unwrap_or(-1),
            "ASC",
        ),
        ScanOrder::Descending => (
            "id < ?",
            start.after.map(|after| after as i64).unwrap_or(i64::MAX),
            "DESC",
        ),
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct DocumentAction {
    pub(super) create: Option<DocumentCreate>,
    pub(super) content: Option<DocumentContent>,
    pub(super) copy: Option<crate::types::DocumentCopySource>,
    pub(super) retire: bool,
}

#[derive(Default)]
struct Admission {
    closed: bool,
    admitted_reads: usize,
}

/// Portable SQLite implementation of the storage contract.
pub struct SqliteStorage {
    db: SqliteDatabase,
    pub(super) schema: SqliteSchema,
    next_id: Mutex<u64>,
    admission: Mutex<Admission>,
    drained: tokio::sync::Notify,
    close_lock: tokio::sync::Mutex<()>,
}

/// Keeps one admitted multi-query read alive until it settles so `close()` can
/// wait for it (upstream `admitRead`).
struct ReadGuard<'a> {
    storage: &'a SqliteStorage,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut admission) = self.storage.admission.lock() {
            admission.admitted_reads = admission.admitted_reads.saturating_sub(1);
            if admission.admitted_reads == 0 {
                drop(admission);
                self.storage.drained.notify_waiters();
            }
        }
    }
}

/// Storage-level SQLite options.
#[derive(Debug, Clone, Default)]
pub struct SqliteStorageOptions {
    /// Prefix for every table and index in the durable schema. It must be a
    /// SQLite identifier and must not use SQLite's reserved `sqlite_` prefix.
    /// Existing data is not migrated when this changes.
    pub prefix: Option<String>,
}

impl SqliteStorage {
    /// Initialize storage over an owned SQLite database facade.
    pub async fn open(db: SqliteDatabase) -> StorageResult<Self> {
        Self::open_with_options(db, SqliteStorageOptions::default()).await
    }

    /// Initialize storage over an owned SQLite database facade under optional
    /// table/index identifiers (upstream `SqliteStorage.open(db, options)`).
    pub async fn open_with_options(
        db: SqliteDatabase,
        options: SqliteStorageOptions,
    ) -> StorageResult<Self> {
        match Self::open_inner(&db, &options).await {
            Ok(storage) => Ok(storage),
            Err(error) => {
                let _ = db.close().await;
                Err(error)
            }
        }
    }

    async fn open_inner(
        db: &SqliteDatabase,
        options: &SqliteStorageOptions,
    ) -> StorageResult<Self> {
        let schema = create_sqlite_schema(options.prefix.as_deref())?;
        apply_sqlite_migrations_for_schema(db, &schema, &create_sqlite_migrations(&schema)).await?;
        let metadata = format!(
            "SELECT next_id FROM {} WHERE singleton = 1",
            schema.tables.durable_metadata
        );
        let next_id = db
            .get_text(&metadata, vec![])
            .await?
            .ok_or_else(|| StorageError::message("Durable SQLite metadata is missing"))?
            .parse::<u64>()
            .map_err(|_| StorageError::message("Durable SQLite metadata is missing"))?;
        Ok(Self {
            db: db.clone_handle(),
            schema,
            next_id: Mutex::new(next_id),
            admission: Mutex::new(Admission::default()),
            drained: tokio::sync::Notify::new(),
            close_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Identifiers this storage uses for its tables and indexes.
    pub fn schema(&self) -> &SqliteSchema {
        &self.schema
    }

    fn assert_open(&self) -> StorageResult<()> {
        let admission = self
            .admission
            .lock()
            .map_err(|_| StorageError::message("SqliteStorage admission lock poisoned"))?;
        if admission.closed {
            return Err(StorageError::Closed("SqliteStorage"));
        }
        Ok(())
    }

    /// Admit one multi-query read; `close()` drains these before it closes the
    /// database (upstream `admitRead`).
    fn admit_read(&self) -> StorageResult<ReadGuard<'_>> {
        let mut admission = self
            .admission
            .lock()
            .map_err(|_| StorageError::message("SqliteStorage admission lock poisoned"))?;
        if admission.closed {
            return Err(StorageError::Closed("SqliteStorage"));
        }
        admission.admitted_reads += 1;
        Ok(ReadGuard { storage: self })
    }

    fn next_id_snapshot(&self) -> StorageResult<u64> {
        self.next_id
            .lock()
            .map(|guard| *guard)
            .map_err(|_| StorageError::message("SqliteStorage ID lock poisoned"))
    }

    fn advance_next_id(&self, candidate: u64) -> StorageResult<()> {
        let mut guard = self
            .next_id
            .lock()
            .map_err(|_| StorageError::message("SqliteStorage ID lock poisoned"))?;
        *guard = (*guard).max(candidate);
        Ok(())
    }

    async fn read_conversation(
        &self,
        id: ConversationId,
    ) -> StorageResult<Option<ConversationRecord>> {
        self.db
            .get(
                &format!(
                    "SELECT record FROM {} WHERE id = ?",
                    self.schema.tables.conversations
                ),
                vec![id.get().into()],
            )
            .await
    }

    async fn read_entry(
        &self,
        id: EntryId,
        conversation_id: Option<ConversationId>,
    ) -> StorageResult<Option<StoredEntry>> {
        let mut conversation = None;
        if let Some(conversation_id) = conversation_id {
            conversation = self.read_conversation(conversation_id).await?;
            if conversation.is_none() {
                return Err(StorageError::UnknownConversation(conversation_id));
            }
        }
        let id_value = id.get();
        let sql = format!(
            "SELECT record, commit_seq FROM {} WHERE id = ?",
            self.schema.tables.entries
        );
        let row: Option<(String, i64)> = self
            .db
            .with_connection(move |conn| {
                let mut statement = conn.prepare_cached(&sql)?;
                let mut rows = statement.query(rusqlite::params![id_value as i64])?;
                match rows.next()? {
                    None => Ok(None),
                    Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
                }
            })
            .await?;
        let Some((record, commit_seq)) = row else {
            return Ok(None);
        };
        let entry: EntryRecord = serde_json::from_str(&record)?;
        if let Some(mut conversation) = conversation {
            let mut upper = u64::MAX;
            while conversation.id != entry.conversation_id {
                let Some(parent) = conversation.parent.clone() else {
                    return Ok(None);
                };
                upper = upper.min(parent.at.get());
                conversation = self
                    .read_conversation(parent.conversation_id)
                    .await?
                    .ok_or_else(|| StorageError::UnknownConversation(parent.conversation_id))?;
            }
            if entry.id.get() > upper {
                return Ok(None);
            }
        }
        Ok(Some(StoredEntry {
            entry,
            commit_seq: Seq(commit_seq as u64),
        }))
    }

    async fn read_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
    ) -> StorageResult<Option<EntryRecord>> {
        let mut conversation = self
            .read_conversation(conversation_id)
            .await?
            .ok_or(StorageError::UnknownConversation(conversation_id))?;
        let mut upper = at_or_before_entry_id.map(EntryId::get);
        let entries = &self.schema.tables.entries;
        let latest = format!(
            "SELECT record FROM {entries} WHERE conversation_id = ? AND head IS NOT NULL ORDER BY id DESC LIMIT 1"
        );
        let at_or_before = format!(
            "SELECT record FROM {entries} WHERE conversation_id = ? AND head IS NOT NULL AND id <= ? ORDER BY id DESC LIMIT 1"
        );
        loop {
            let record: Option<EntryRecord> = match upper {
                None => {
                    self.db
                        .get(&latest, vec![conversation.id.get().into()])
                        .await?
                }
                Some(upper) => {
                    self.db
                        .get(
                            &at_or_before,
                            vec![conversation.id.get().into(), upper.into()],
                        )
                        .await?
                }
            };
            if record.is_some() {
                return Ok(record);
            }
            let Some(parent) = conversation.parent.clone() else {
                return Ok(None);
            };
            upper = Some(upper.map_or(parent.at.get(), |upper| upper.min(parent.at.get())));
            conversation = self
                .read_conversation(parent.conversation_id)
                .await?
                .ok_or(StorageError::UnknownConversation(parent.conversation_id))?;
        }
    }

    async fn read_entries_descending(
        &self,
        query: &EntryQuery,
        limit: usize,
        after: Option<u64>,
    ) -> StorageResult<Page<EntryRecord>> {
        let mut conversation = self
            .read_conversation(query.conversation_id)
            .await?
            .ok_or(StorageError::UnknownConversation(query.conversation_id))?;
        let mut upper = query.max_entry_id.map(EntryId::get);
        if let Some(after) = after {
            upper = Some(upper.map_or(after.saturating_sub(1), |upper| {
                upper.min(after.saturating_sub(1))
            }));
        }
        let mut values: Vec<EntryRecord> = Vec::new();
        loop {
            let mut clauses = vec!["conversation_id = ?".to_string()];
            let mut params: Vec<SqliteValue> = vec![conversation.id.get().into()];
            if let Some(min) = query.min_entry_id {
                clauses.push("id >= ?".to_string());
                params.push(min.get().into());
            }
            if let Some(upper) = upper {
                clauses.push("id <= ?".to_string());
                params.push(upper.into());
            }
            params.push((limit + 1 - values.len()).into());
            let sql = format!(
                "SELECT record FROM {} WHERE {} ORDER BY id DESC LIMIT ?",
                self.schema.tables.entries,
                clauses.join(" AND ")
            );
            let rows: Vec<EntryRecord> = self.db.all(&sql, params).await?;
            values.extend(rows);
            if values.len() > limit {
                break;
            }
            let Some(parent) = conversation.parent.clone() else {
                break;
            };
            upper = Some(upper.map_or(parent.at.get(), |upper| upper.min(parent.at.get())));
            if let Some(min) = query.min_entry_id
                && upper.expect("set") < min.get()
            {
                break;
            }
            conversation = self
                .read_conversation(parent.conversation_id)
                .await?
                .ok_or(StorageError::UnknownConversation(parent.conversation_id))?;
        }
        Ok(page_values(
            values,
            limit,
            ScanOrder::Descending,
            |entry: &EntryRecord| entry.id.get(),
        ))
    }

    /// Oldest first: the fork chain's segments from the root conversation
    /// forward, each up to its fork point.
    async fn read_entries_ascending(
        &self,
        query: &EntryQuery,
        limit: usize,
        after: Option<u64>,
    ) -> StorageResult<Page<EntryRecord>> {
        let mut segments: Vec<(ConversationId, Option<u64>)> = Vec::new();
        let mut conversation = self
            .read_conversation(query.conversation_id)
            .await?
            .ok_or(StorageError::UnknownConversation(query.conversation_id))?;
        let mut upper = query.max_entry_id.map(EntryId::get);
        loop {
            segments.push((conversation.id, upper));
            let Some(parent) = conversation.parent.clone() else {
                break;
            };
            upper = Some(upper.map_or(parent.at.get(), |upper| upper.min(parent.at.get())));
            if let Some(min) = query.min_entry_id
                && upper.expect("set") < min.get()
            {
                break;
            }
            conversation = self
                .read_conversation(parent.conversation_id)
                .await?
                .ok_or(StorageError::UnknownConversation(parent.conversation_id))?;
        }
        let mut lower = query.min_entry_id.map(EntryId::get);
        if let Some(after) = after {
            lower = Some(lower.map_or(after + 1, |lower| lower.max(after + 1)));
        }
        let mut values: Vec<EntryRecord> = Vec::new();
        for (segment_id, segment_upper) in segments.into_iter().rev() {
            let mut clauses = vec!["conversation_id = ?".to_string()];
            let mut params: Vec<SqliteValue> = vec![segment_id.get().into()];
            if let Some(lower) = lower {
                clauses.push("id >= ?".to_string());
                params.push(lower.into());
            }
            if let Some(upper) = segment_upper {
                clauses.push("id <= ?".to_string());
                params.push(upper.into());
            }
            params.push((limit + 1 - values.len()).into());
            let sql = format!(
                "SELECT record FROM {} WHERE {} ORDER BY id ASC LIMIT ?",
                self.schema.tables.entries,
                clauses.join(" AND ")
            );
            let rows: Vec<EntryRecord> = self.db.all(&sql, params).await?;
            values.extend(rows);
            if values.len() > limit {
                break;
            }
        }
        Ok(page_values(
            values,
            limit,
            ScanOrder::Ascending,
            |entry: &EntryRecord| entry.id.get(),
        ))
    }

    fn candidate_next_id(&self, writes: &[StorageWrite]) -> StorageResult<u64> {
        let mut next = self.next_id_snapshot()?;
        for write in writes {
            if let Some(id) = write.write_id() {
                next = next.max(id + 1);
            }
        }
        Ok(next)
    }

    fn prepare_document_actions(
        writes: &[StorageWrite],
    ) -> StorageResult<Vec<(DocumentId, DocumentAction)>> {
        let mut actions: Vec<(DocumentId, DocumentAction)> = Vec::new();
        let mut index_of: std::collections::HashMap<DocumentId, usize> =
            std::collections::HashMap::new();
        for write in writes {
            let id = match write {
                StorageWrite::DocumentCreate { record, .. }
                | StorageWrite::DocumentCopy { record, .. } => record.id,
                StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => {
                    *id
                }
                _ => continue,
            };
            let position = *index_of.entry(id).or_insert_with(|| {
                actions.push((id, DocumentAction::default()));
                actions.len() - 1
            });
            let action = &mut actions[position].1;
            match write {
                StorageWrite::DocumentCreate { record, content } => {
                    if action.create.is_some() || action.content.is_some() || action.copy.is_some()
                    {
                        return Err(StorageError::message(format!(
                            "Document {id} has more than one content command"
                        )));
                    }
                    action.create = Some(record.clone());
                    action.content = Some(content.clone());
                }
                StorageWrite::DocumentCopy { record, source } => {
                    if action.create.is_some() || action.content.is_some() || action.copy.is_some()
                    {
                        return Err(StorageError::message(format!(
                            "Document {id} has more than one content command"
                        )));
                    }
                    action.create = Some(record.clone());
                    action.copy = Some(source.clone());
                }
                StorageWrite::DocumentChange { content, .. } => {
                    if action.content.is_some() || action.copy.is_some() {
                        return Err(StorageError::message(format!(
                            "Document {id} has more than one content command"
                        )));
                    }
                    action.content = Some(content.clone());
                }
                StorageWrite::DocumentRetire { .. } => {
                    if action.retire {
                        return Err(StorageError::message(format!(
                            "Document {id} is retired more than once"
                        )));
                    }
                    action.retire = true;
                }
                _ => {
                    return Err(StorageError::message(
                        "unsupported write variant in the document-action branch",
                    ));
                }
            }
        }
        Ok(actions)
    }

    pub(super) async fn materialize_document(
        &self,
        tx: &SqliteTransaction,
        id: DocumentId,
        at: DocumentPoint,
    ) -> StorageResult<Option<StoredDocument>> {
        let row: Option<DocumentRecord> = tx
            .get(
                &format!(
                    "SELECT record FROM {} WHERE id = ?",
                    self.schema.tables.documents
                ),
                vec![id.get().into()],
            )
            .await?;
        let Some(record) = row else {
            return Ok(None);
        };
        if !at.is_current() && is_current_only(&record) {
            return Err(StorageError::message(format!(
                "Document {id} does not retain historical content"
            )));
        }
        if !is_alive_at(&record, at) {
            return Ok(None);
        }
        // Bound the revision scan by the selected sequence; `current` maps to
        // the maximum representable SQLite integer.
        let upper = at.upper().min(i64::MAX as u64) as i64;
        let id_value = id.get();
        let base_sql = format!(
            "SELECT seq, kind, version, content FROM {}
						WHERE document_id = ? AND kind = 'base' AND seq <= ? ORDER BY seq DESC LIMIT 1",
            self.schema.tables.document_revisions
        );
        let base: Option<(i64, String, i64, String)> = tx
            .with_connection(move |conn| {
                let mut statement = conn.prepare_cached(&base_sql)?;
                let mut rows = statement.query(rusqlite::params![id_value as i64, upper])?;
                match rows.next()? {
                    None => Ok(None),
                    Some(row) => Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))),
                }
            })
            .await?;
        let Some((base_seq, _, base_version, base_content)) = base else {
            return Err(StorageError::message(format!(
                "Document {id} is missing a required base"
            )));
        };
        let value: JsonObject = serde_json::from_str(&base_content)?;
        let tail_sql = format!(
            "SELECT seq, kind, version, content FROM {}
						WHERE document_id = ? AND seq > ? AND seq <= ? ORDER BY seq",
            self.schema.tables.document_revisions
        );
        let tail: Vec<(i64, String, i64, String)> = tx
            .with_connection(move |conn| {
                let mut statement = conn.prepare_cached(&tail_sql)?;
                let mut rows =
                    statement.query(rusqlite::params![id_value as i64, base_seq, upper])?;
                let mut values = Vec::new();
                while let Some(row) = rows.next()? {
                    values.push((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?));
                }
                Ok(values)
            })
            .await?;
        let mut ops: Vec<Vec<Op>> = Vec::new();
        for (_, kind, version, content) in &tail {
            if kind != "delta" || *version as u64 != base_version as u64 {
                return Err(StorageError::message(format!(
                    "Document {id} crosses a stored version boundary without a base"
                )));
            }
            ops.push(serde_json::from_str::<Vec<Op>>(content)?);
        }
        let value = apply_immutable_batches(
            Some(&JsonValue::Object(value)),
            ops.iter().map(Vec::as_slice),
        )?;
        let JsonValue::Object(value) = value else {
            return Err(StorageError::message(format!(
                "Document {id} root is not an object"
            )));
        };
        Ok(Some(StoredDocument {
            record,
            version: base_version as u64,
            value,
            deltas_since_base: tail.len() as u64,
        }))
    }
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn commit(
        &self,
        batches: &[Vec<StorageWrite>],
        receive: &mut (dyn FnMut(usize, Seq) + Send),
        _context: &Context,
    ) -> StorageResult<crate::types::CommitResult> {
        self.assert_open()?;
        let batches = batches.to_vec();
        let (seqs, refused, candidate_next_id) = self
            .db
            .transaction(move |tx| async move {
                let tables = &self.schema.tables;
                let metadata = &tables.durable_metadata;
                let metadata_next_id = tx
                    .get_text(
                        &format!("SELECT next_id FROM {metadata} WHERE singleton = 1"),
                        vec![],
                    )
                    .await?
                    .ok_or_else(|| StorageError::message("Durable SQLite metadata is missing"))?
                    .parse::<u64>()
                    .map_err(|_| StorageError::message("Durable SQLite metadata is missing"))?;
                let mut next_seq = tx
                    .get_i64(
                        &format!("SELECT next_seq FROM {metadata} WHERE singleton = 1"),
                        vec![],
                    )
                    .await?
                    .ok_or_else(|| StorageError::message("Durable SQLite metadata is missing"))?
                    as u64;
                // D02: read every batch's check state before checking any batch.
                // A malformed document record fails the whole call (Err), not a
                // refusal.
                let mut state = CommitState::read(&tx, &self, &batches).await?;
                let mut pending = PendingWrites::default();
                let mut seqs = Vec::new();
                let mut refused: Option<CommitRefusal> = None;
                let mut candidate_next_id = self.next_id_snapshot()?;
                for writes in &batches {
                    let mut actions = match Self::prepare_document_actions(writes) {
                        Ok(actions) => actions,
                        Err(error) => {
                            refused = Some(CommitRefusal::from_storage(error));
                            break;
                        }
                    };
                    if let Err(error) = state.check(writes, &actions) {
                        refused = Some(error);
                        break;
                    }
                    // Copies read their sources from this transaction, including
                    // the accepted batches before them.
                    if actions.iter().any(|(_, action)| action.copy.is_some()) {
                        pending.flush(&tx, &self.schema.tables).await?;
                        match self.resolve_copies(&tx, &mut actions).await {
                            Ok(()) => {}
                            Err(CopyResolution::Refused(error)) => {
                                refused = Some(error);
                                break;
                            }
                            Err(CopyResolution::Storage(error)) => return Err(error),
                        }
                    }
                    let seq = Seq(next_seq);
                    next_seq += 1;
                    state.apply(writes, &actions, seq);
                    pending.add(writes, &actions, seq, &state)?;
                    seqs.push(seq);
                    candidate_next_id = candidate_next_id.max(self.candidate_next_id(writes)?);
                }
                pending.flush(&tx, &self.schema.tables).await?;
                if !seqs.is_empty() {
                    tx.run(
                        &format!(
                            "UPDATE {metadata} SET next_id = ?, next_seq = ? WHERE singleton = 1"
                        ),
                        vec![
                            metadata_next_id.max(candidate_next_id).to_string().into(),
                            next_seq.into(),
                        ],
                    )
                    .await?;
                }
                Ok((seqs, refused.map(|refusal| refusal.0), candidate_next_id))
            })
            .await?;
        self.advance_next_id(candidate_next_id)?;
        for (index, seq) in seqs.iter().copied().enumerate() {
            receive(index, seq);
        }
        Ok(crate::types::CommitResult {
            stored: seqs.len(),
            refused,
        })
    }

    async fn mint_id(&self) -> StorageResult<u64> {
        self.assert_open()?;
        let mut guard = self
            .next_id
            .lock()
            .map_err(|_| StorageError::message("SqliteStorage ID lock poisoned"))?;
        let id = *guard;
        if id > MAX_SAFE_INTEGER {
            return Err(StorageError::IdSpaceExhausted);
        }
        *guard += 1;
        Ok(id)
    }

    async fn conversation(
        &self,
        id: ConversationId,
        _context: &Context,
    ) -> StorageResult<Option<Arc<ConversationRecord>>> {
        self.assert_open()?;
        self.read_conversation(id)
            .await
            .map(|value| value.map(Arc::new))
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &Context,
    ) -> StorageResult<Page<ConversationRecord>> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
        let (clause, param, direction) = scan_sql(start);
        let mut clauses = vec![clause.to_string()];
        let mut params: Vec<SqliteValue> = vec![param.into()];
        if let Some(owner) = query.owner_conversation_id {
            clauses.push("owner_conversation_id = ?".to_string());
            params.push(owner.get().into());
        }
        if let Some(task) = query.owner_task_id {
            clauses.push("owner_task_id = ?".to_string());
            params.push(task.get().into());
        }
        params.push((limit + 1).into());
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id {} LIMIT ?",
            self.schema.tables.conversations,
            clauses.join(" AND "),
            direction
        );
        let rows: Vec<ConversationRecord> = self.db.all(&sql, params).await?;
        Ok(page_values(
            rows,
            limit,
            start.order,
            |record: &ConversationRecord| record.id.get(),
        ))
    }

    async fn entry(&self, id: EntryId, _context: &Context) -> StorageResult<Option<StoredEntry>> {
        let _guard = self.admit_read()?;
        self.read_entry(id, None).await
    }

    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        _context: &Context,
    ) -> StorageResult<Option<StoredEntry>> {
        let _guard = self.admit_read()?;
        self.read_entry(id, Some(conversation_id)).await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        _context: &Context,
    ) -> StorageResult<Option<EntryRecord>> {
        let _guard = self.admit_read()?;
        self.read_latest_head_marker(conversation_id, at_or_before_entry_id)
            .await
    }

    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &Context,
    ) -> StorageResult<Page<EntryRecord>> {
        let _guard = self.admit_read()?;
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Descending)?;
        if start.order == ScanOrder::Ascending {
            self.read_entries_ascending(&query, limit, start.after)
                .await
        } else {
            self.read_entries_descending(&query, limit, start.after)
                .await
        }
    }

    async fn task(&self, id: TaskId, _context: &Context) -> StorageResult<Option<Arc<TaskRecord>>> {
        self.assert_open()?;
        let value: Option<TaskRecord> = self
            .db
            .get(
                &format!(
                    "SELECT record FROM {} WHERE id = ?",
                    self.schema.tables.tasks
                ),
                vec![id.get().into()],
            )
            .await?;
        Ok(value.map(Arc::new))
    }

    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &Context,
    ) -> StorageResult<Page<TaskRecord>> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
        let (clause, param, direction) = scan_sql(start);
        let mut clauses = vec![clause.to_string()];
        let mut params: Vec<SqliteValue> = vec![param.into()];
        if let Some(conversation) = query.conversation_id {
            clauses.push("conversation_id = ?".to_string());
            params.push(conversation.get().into());
        }
        if let Some(kind) = &query.kind {
            clauses.push("kind = ?".to_string());
            params.push(encode_indexed_string(kind).into());
        }
        if let Some(status) = query.status {
            clauses.push("status = ?".to_string());
            params.push(status.as_str().into());
        }
        if let Some(abort_requested) = query.abort_requested {
            clauses.push("abort_requested = ?".to_string());
            params.push(abort_requested.into());
        }
        if let Some(background) = query.background {
            clauses.push("background = ?".to_string());
            params.push(background.into());
        }
        params.push((limit + 1).into());
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id {} LIMIT ?",
            self.schema.tables.tasks,
            clauses.join(" AND "),
            direction
        );
        let rows: Vec<TaskRecord> = self.db.all(&sql, params).await?;
        Ok(page_values(
            rows,
            limit,
            start.order,
            |task: &TaskRecord| task.id.get(),
        ))
    }

    async fn submission(
        &self,
        id: SubmissionId,
        _context: &Context,
    ) -> StorageResult<Option<Arc<SubmissionRecord>>> {
        self.assert_open()?;
        let value: Option<SubmissionRecord> = self
            .db
            .get(
                &format!(
                    "SELECT record FROM {} WHERE id = ?",
                    self.schema.tables.submissions
                ),
                vec![id.get().into()],
            )
            .await?;
        Ok(value.map(Arc::new))
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &Context,
    ) -> StorageResult<Page<SubmissionRecord>> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)?;
        let (clause, param, direction) = scan_sql(start);
        let mut clauses = vec![clause.to_string()];
        let mut params: Vec<SqliteValue> = vec![param.into()];
        if let Some(conversation) = query.conversation_id {
            clauses.push("conversation_id = ?".to_string());
            params.push(conversation.get().into());
        }
        if let Some(status) = query.status {
            clauses.push("status = ?".to_string());
            params.push(status.as_str().into());
        }
        params.push((limit + 1).into());
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id {} LIMIT ?",
            self.schema.tables.submissions,
            clauses.join(" AND "),
            direction
        );
        let rows: Vec<SubmissionRecord> = self.db.all(&sql, params).await?;
        Ok(page_values(
            rows,
            limit,
            start.order,
            |record: &SubmissionRecord| record.id().get(),
        ))
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        _context: &Context,
    ) -> StorageResult<Option<Arc<SubmissionRecord>>> {
        self.assert_open()?;
        let value: Option<SubmissionRecord> = self
            .db
            .get(
                &format!(
                    "SELECT record FROM {} WHERE conversation_id = ? AND request_id = ?",
                    self.schema.tables.submissions
                ),
                vec![
                    conversation_id.get().into(),
                    encode_indexed_string(request_id).into(),
                ],
            )
            .await?;
        Ok(value.map(Arc::new))
    }

    async fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
        _context: &Context,
    ) -> StorageResult<Option<DocumentRecord>> {
        self.assert_open()?;
        let parts = address_parts_of(&address);
        let params: Vec<SqliteValue> = vec![
            parts.kind.as_str().into(),
            parts.scope_kind.into(),
            parts.owner_id.into(),
            parts.family.into(),
            parts.key_value.as_str().into(),
        ];
        let documents = &self.schema.tables.documents;
        let sql = if at.is_current() {
            format!(
                "SELECT record FROM {documents}
				WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
				AND retired_at IS NULL ORDER BY created_at DESC LIMIT 1"
            )
        } else {
            format!(
                "SELECT record FROM {documents}
				WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
				AND created_at <= ? AND (retired_at IS NULL OR retired_at > ?)
				ORDER BY created_at DESC LIMIT 1"
            )
        };
        if at.is_current() {
            self.db.get(&sql, params).await
        } else {
            let bound = at.upper();
            let mut params = params;
            params.push(bound.into());
            params.push(bound.into());
            self.db.get(&sql, params).await
        }
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        _context: &Context,
    ) -> StorageResult<Option<StoredDocument>> {
        self.assert_open()?;
        // The record and revision queries must observe one committed state; a
        // commit between them can replace the base.
        self.db
            .transaction(move |tx| async move { self.materialize_document(&tx, id, at).await })
            .await
    }

    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &Context,
    ) -> StorageResult<Page<DocumentRecord>> {
        self.assert_open()?;
        crate::storage::validate_cursor(cursor.as_ref())?;
        let scope = scope_columns(&query.scope);
        let mut clauses = vec![
            "scope_kind = ?".to_string(),
            "owner_id = ?".to_string(),
            "id > ?".to_string(),
        ];
        let cursor_after = cursor
            .as_ref()
            .map(|cursor| cursor.after as i64)
            .unwrap_or(-1);
        let mut params: Vec<SqliteValue> = vec![
            scope.scope_kind.into(),
            scope.owner_id.into(),
            cursor_after.into(),
        ];
        if let Some(kind) = &query.kind {
            clauses.push("kind = ?".to_string());
            params.push(encode_indexed_string(kind).into());
        }
        if query.at.is_current() {
            clauses.push("retired_at IS NULL".to_string());
        } else {
            clauses.push("created_at <= ?".to_string());
            clauses.push("(retired_at IS NULL OR retired_at > ?)".to_string());
            params.push(query.at.upper().into());
            params.push(query.at.upper().into());
        }
        params.push((limit + 1).into());
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id LIMIT ?",
            self.schema.tables.documents,
            clauses.join(" AND ")
        );
        let rows: Vec<DocumentRecord> = self.db.all(&sql, params).await?;
        Ok(page_values(
            rows,
            limit,
            ScanOrder::Ascending,
            |record: &DocumentRecord| record.id.get(),
        ))
    }

    async fn close(&self, _context: &Context) -> StorageResult<()> {
        // Repeated closes coalesce: a second caller settles only after the
        // first finished closing the database (upstream shares one promise).
        let _close_guard = self.close_lock.lock().await;
        {
            let mut admission = self
                .admission
                .lock()
                .map_err(|_| StorageError::message("SqliteStorage admission lock poisoned"))?;
            admission.closed = true;
        }
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            // Register before checking: `notify_waiters` stores no permit.
            notified.as_mut().enable();
            let admitted = self
                .admission
                .lock()
                .map_err(|_| StorageError::message("SqliteStorage admission lock poisoned"))?
                .admitted_reads;
            if admitted == 0 {
                break;
            }
            notified.await;
        }
        self.db.close().await
    }
}
