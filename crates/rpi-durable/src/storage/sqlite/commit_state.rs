//! D02 commit pre-read and in-memory checks (upstream `CommitState`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::chord::json::JsonValue;
use crate::chord::sqlite::database::{SqliteTransaction, SqliteValue};
use crate::errors::{StorageError, StorageResult};
use crate::ids::{DocumentId, Seq};
use crate::types::{DocumentContent, DocumentRecord, DocumentScope, StorageWrite};

use super::storage::{AddressParts, DocumentAction, SqliteStorage, address_parts};

/// A Durable validation refusal. Only these errors become `CommitResult.refused`;
/// database and JSON-parse failures stay `StorageError`.
#[derive(Debug)]
pub(super) struct CommitRefusal(pub StorageError);

impl CommitRefusal {
    pub(super) fn message(value: impl Into<String>) -> Self {
        Self(StorageError::message(value))
    }

    pub(super) fn request(value: impl Into<String>) -> Self {
        Self(StorageError::Request(
            crate::errors::StorageRequestError::new(value),
        ))
    }

    pub(super) fn from_storage(error: StorageError) -> Self {
        Self(error)
    }
}

pub(super) type AddressKey = (String, String, i64, i64, String);

fn address_key(parts: &AddressParts) -> AddressKey {
    (
        parts.kind.clone(),
        parts.scope_kind.to_string(),
        parts.owner_id,
        parts.family,
        parts.key_value.clone(),
    )
}

fn address_key_of(record: &DocumentRecord) -> AddressKey {
    address_key(&address_parts(
        &record.kind,
        &record.scope,
        record.key.as_deref(),
    ))
}

/// The check state of every batch in one `commit` call, read before any check.
#[derive(Default)]
pub(super) struct CommitState {
    record_ids: HashMap<u64, String>,
    documents: HashMap<u64, DocumentRecord>,
    versions: HashMap<u64, u64>,
    current: HashMap<AddressKey, Option<u64>>,
}

impl CommitState {
    pub(super) async fn read(
        tx: &SqliteTransaction,
        storage: &SqliteStorage,
        batches: &[Vec<StorageWrite>],
    ) -> StorageResult<Self> {
        let tables = &storage.schema.tables;
        let mut ids = BTreeSet::new();
        let mut document_ids = BTreeSet::new();
        let mut delta_ids = BTreeSet::new();
        let mut addresses: BTreeMap<AddressKey, [JsonValue; 5]> = BTreeMap::new();
        for writes in batches {
            for write in writes {
                if let Some(id) = write.write_id() {
                    ids.insert(id);
                }
                match write {
                    StorageWrite::DocumentCreate { record, .. }
                    | StorageWrite::DocumentCopy { record, .. } => {
                        document_ids.insert(record.id.get());
                        let parts =
                            address_parts(&record.kind, &record.scope, record.key.as_deref());
                        addresses.insert(
                            address_key(&parts),
                            [
                                JsonValue::String(parts.kind.clone()),
                                JsonValue::String(parts.scope_kind.to_string()),
                                JsonValue::from(parts.owner_id),
                                JsonValue::from(parts.family),
                                JsonValue::String(parts.key_value.clone()),
                            ],
                        );
                    }
                    StorageWrite::DocumentChange { id, content } => {
                        document_ids.insert(id.get());
                        if matches!(content, DocumentContent::Delta { .. }) {
                            delta_ids.insert(id.get());
                        }
                    }
                    StorageWrite::DocumentRetire { id } => {
                        document_ids.insert(id.get());
                    }
                    _ => {}
                }
            }
        }
        let record_ids = read_record_ids(tx, &tables.record_ids, &ids).await?;
        let documents = read_documents(tx, &tables.documents, &document_ids).await?;
        let versions = read_versions(tx, &tables.document_revisions, &delta_ids).await?;
        let current = read_current(tx, &tables.documents, &addresses).await?;
        Ok(Self {
            record_ids,
            documents,
            versions,
            current,
        })
    }

    pub(super) fn check(
        &self,
        writes: &[StorageWrite],
        actions: &[(DocumentId, DocumentAction)],
    ) -> Result<(), CommitRefusal> {
        self.check_ids(writes)?;
        self.check_documents(actions)
    }

    fn check_ids(&self, writes: &[StorageWrite]) -> Result<(), CommitRefusal> {
        let mut claimed: HashMap<u64, &'static str> = HashMap::new();
        for write in writes {
            let (Some(id), Some(table)) = (write.write_id(), write.table_name()) else {
                continue;
            };
            let existing = self.record_ids.get(&id).map(String::as_str);
            let earlier = claimed.get(&id).copied();
            if matches!(table, "conversation" | "entry" | "document") {
                if let Some(existing) = existing {
                    return Err(CommitRefusal::message(format!(
                        "ID {id} already belongs to {existing}"
                    )));
                }
                if earlier.is_some() {
                    return Err(CommitRefusal::message(format!(
                        "ID {id} is written more than once"
                    )));
                }
            } else {
                if let Some(existing) = existing
                    && existing != table
                {
                    return Err(CommitRefusal::message(format!(
                        "ID {id} already belongs to {existing}"
                    )));
                }
                if let Some(earlier) = earlier
                    && earlier != table
                {
                    return Err(CommitRefusal::message(format!(
                        "ID {id} is written as two record types"
                    )));
                }
            }
            claimed.insert(id, table);
        }
        Ok(())
    }

    fn check_documents(
        &self,
        actions: &[(DocumentId, DocumentAction)],
    ) -> Result<(), CommitRefusal> {
        let mut live_counts: HashMap<AddressKey, i64> = HashMap::new();
        for (id, action) in actions {
            if let Some(copy) = &action.copy
                && actions.iter().any(|(other, _)| other == &copy.id)
            {
                return Err(CommitRefusal::request(format!(
                    "Document copy {id} source is changed in the copy batch"
                )));
            }
            let existing = self.documents.get(&id.get());
            if action.create.is_none() && existing.is_none() {
                return Err(CommitRefusal::message(format!("Unknown document: {id}")));
            }
            if action.create.is_some() && existing.is_some() {
                return Err(CommitRefusal::message(format!(
                    "Document {id} already exists"
                )));
            }
            if existing.is_some_and(|record| record.retired_at.is_some()) {
                return Err(CommitRefusal::message(format!("Document {id} is retired")));
            }
            if let Some(DocumentContent::Delta { version, .. }) = &action.content {
                let Some(previous) = self.versions.get(&id.get()) else {
                    return Err(CommitRefusal::message(format!(
                        "Document {id} delta has no base"
                    )));
                };
                if previous != version {
                    return Err(CommitRefusal::message(format!(
                        "Document {id} version transition requires a base"
                    )));
                }
            }
            let parts = match (&action.create, existing) {
                (Some(create), _) => {
                    address_parts(&create.kind, &create.scope, create.key.as_deref())
                }
                (None, Some(existing)) => {
                    address_parts(&existing.kind, &existing.scope, existing.key.as_deref())
                }
                (None, None) => {
                    return Err(CommitRefusal::message(format!(
                        "Document {id} has neither a create record nor an existing version"
                    )));
                }
            };
            let key = address_key(&parts);
            let current = self.current.get(&key).copied().flatten().is_some();
            let mut live = live_counts.get(&key).copied().unwrap_or(i64::from(current));
            if action.retire && existing.is_some() {
                live -= 1;
            }
            if action.create.is_some() && !action.retire {
                live += 1;
            }
            live_counts.insert(key, live);
        }
        if live_counts.values().any(|live| *live > 1) {
            return Err(CommitRefusal::message(
                "Document address already has a current incarnation",
            ));
        }
        Ok(())
    }

    pub(super) fn apply(
        &mut self,
        writes: &[StorageWrite],
        actions: &[(DocumentId, DocumentAction)],
        seq: Seq,
    ) {
        for write in writes {
            if let (Some(id), Some(table)) = (write.write_id(), write.table_name()) {
                self.record_ids.insert(id, table.to_string());
            }
        }
        for (id, action) in actions {
            if let Some(create) = &action.create {
                let record = DocumentRecord {
                    id: *id,
                    kind: create.kind.clone(),
                    key: create.key.clone(),
                    created_at: seq,
                    retired_at: action.retire.then_some(seq),
                    scope: create.scope.clone(),
                    history: create.history,
                    fork: create.fork,
                };
                self.documents.insert(id.get(), record.clone());
                if !action.retire {
                    self.current.insert(address_key_of(&record), Some(id.get()));
                }
            } else if action.retire
                && let Some(record) = self.documents.get_mut(&id.get())
            {
                record.retired_at = Some(seq);
                let key = address_key_of(record);
                if self.current.get(&key).copied().flatten() == Some(id.get()) {
                    self.current.insert(key, None);
                }
            }
            if let Some(content) = &action.content {
                self.versions.insert(id.get(), content.version());
            }
        }
    }
}

async fn read_record_ids(
    tx: &SqliteTransaction,
    table: &str,
    ids: &BTreeSet<u64>,
) -> StorageResult<HashMap<u64, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql =
        format!("SELECT id, record_type FROM {table} WHERE id IN (SELECT value FROM json_each(?))");
    let array = serde_json::to_string(&ids.iter().copied().collect::<Vec<_>>())?;
    let rows: Vec<(i64, String)> = tx
        .with_connection(move |conn| {
            let mut statement = conn.prepare_cached(&sql)?;
            let mut result = Vec::new();
            let mut query = statement.query(rusqlite::params![array])?;
            while let Some(row) = query.next()? {
                result.push((row.get(0)?, row.get(1)?));
            }
            Ok(result)
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, table)| (id as u64, table))
        .collect())
}

async fn read_documents(
    tx: &SqliteTransaction,
    table: &str,
    ids: &BTreeSet<u64>,
) -> StorageResult<HashMap<u64, DocumentRecord>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql =
        format!("SELECT id, record FROM {table} WHERE id IN (SELECT value FROM json_each(?))");
    let array = serde_json::to_string(&ids.iter().copied().collect::<Vec<_>>())?;
    let rows: Vec<(i64, String)> = tx
        .with_connection(move |conn| {
            let mut statement = conn.prepare_cached(&sql)?;
            let mut result = Vec::new();
            let mut query = statement.query(rusqlite::params![array])?;
            while let Some(row) = query.next()? {
                result.push((row.get(0)?, row.get(1)?));
            }
            Ok(result)
        })
        .await?;
    let mut documents = HashMap::new();
    for (id, text) in rows {
        // A malformed stored record is a storage failure, not a refusal.
        let record: DocumentRecord = serde_json::from_str(&text)?;
        documents.insert(id as u64, record);
    }
    Ok(documents)
}

async fn read_versions(
    tx: &SqliteTransaction,
    table: &str,
    ids: &BTreeSet<u64>,
) -> StorageResult<HashMap<u64, u64>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT requested.value AS document_id, (
            SELECT version FROM {table} WHERE document_id = requested.value
            ORDER BY seq DESC LIMIT 1
        ) AS version FROM json_each(?) AS requested"
    );
    let array = serde_json::to_string(&ids.iter().copied().collect::<Vec<_>>())?;
    let rows: Vec<(i64, Option<i64>)> = tx
        .with_connection(move |conn| {
            let mut statement = conn.prepare_cached(&sql)?;
            let mut result = Vec::new();
            let mut query = statement.query(rusqlite::params![array])?;
            while let Some(row) = query.next()? {
                result.push((row.get(0)?, row.get(1)?));
            }
            Ok(result)
        })
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, version)| version.map(|version| (id as u64, version as u64)))
        .collect())
}

async fn read_current(
    tx: &SqliteTransaction,
    table: &str,
    addresses: &BTreeMap<AddressKey, [JsonValue; 5]>,
) -> StorageResult<HashMap<AddressKey, Option<u64>>> {
    let mut current: HashMap<AddressKey, Option<u64>> =
        addresses.keys().cloned().map(|key| (key, None)).collect();
    if addresses.is_empty() {
        return Ok(current);
    }
    let sql = format!(
        "SELECT document.id, document.kind, document.scope_kind, document.owner_id,
                document.family, document.key_value
         FROM json_each(?) AS address
         JOIN {table} AS document
           ON document.kind = json_extract(address.value, '$[0]')
          AND document.scope_kind = json_extract(address.value, '$[1]')
          AND document.owner_id = json_extract(address.value, '$[2]')
          AND document.family = json_extract(address.value, '$[3]')
          AND document.key_value = json_extract(address.value, '$[4]')
         WHERE document.retired_at IS NULL"
    );
    let array = serde_json::to_string(
        &addresses
            .values()
            .map(|parts| JsonValue::Array(parts.to_vec()))
            .collect::<Vec<_>>(),
    )?;
    let rows: Vec<(i64, String, String, i64, i64, String)> = tx
        .with_connection(move |conn| {
            let mut statement = conn.prepare_cached(&sql)?;
            let mut result = Vec::new();
            let mut query = statement.query(rusqlite::params![array])?;
            while let Some(row) = query.next()? {
                result.push((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ));
            }
            Ok(result)
        })
        .await?;
    for (id, kind, scope_kind, owner_id, family, key_value) in rows {
        current.insert(
            (kind, scope_kind, owner_id, family, key_value),
            Some(id as u64),
        );
    }
    Ok(current)
}

/// Convert a copy source into a base content, distinguishing refusal from a
/// storage failure (`materialize_document` errors are storage failures).
pub(super) enum CopyResolution {
    Refused(CommitRefusal),
    Storage(StorageError),
}

impl SqliteStorage {
    pub(super) async fn resolve_copies(
        &self,
        tx: &SqliteTransaction,
        actions: &mut [(DocumentId, DocumentAction)],
    ) -> Result<(), CopyResolution> {
        for (id, action) in actions {
            let Some(source) = action.copy.clone() else {
                continue;
            };
            let stored = self
                .materialize_document(tx, source.id, source.at)
                .await
                .map_err(CopyResolution::Storage)?;
            let Some(stored) = stored else {
                return Err(CopyResolution::Refused(CommitRefusal::request(format!(
                    "Fork source document {} cannot be read",
                    source.id
                ))));
            };
            let create = action.create.as_ref().expect("copy carries create");
            if !matches!(stored.record.scope, DocumentScope::Conversation { .. })
                || !matches!(create.scope, DocumentScope::Conversation { .. })
                || stored.record.kind != create.kind
                || stored.record.key != create.key
                || stored.record.history != create.history
                || stored.record.fork != create.fork
            {
                return Err(CopyResolution::Refused(CommitRefusal::request(format!(
                    "Fork source document {} does not match the copied record",
                    source.id
                ))));
            }
            action.content = Some(DocumentContent::Base {
                version: stored.version,
                value: stored.value,
            });
            // The action is now a plain base content; `apply_document_actions`
            // must not resolve the source again.
            action.copy = None;
            let _ = id;
        }
        Ok(())
    }
}

/// Reserved for future multi-row `PendingWrites`; the current commit applies
/// accepted batches through the existing per-table multi-row helpers.
pub(super) fn _unused(_: SqliteValue) {}
