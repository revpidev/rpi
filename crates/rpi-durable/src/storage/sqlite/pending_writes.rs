//! Accepted commit batches accumulated as their net effect per table.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as JsonValue;

use crate::chord::sqlite::database::{SqliteTransaction, SqliteValue};
use crate::errors::{StorageError, StorageResult};
use crate::ids::{DocumentId, Seq};
use crate::storage::is_current_only;
use crate::types::{DocumentContent, DocumentRecord, StorageWrite};

use super::commit_state::CommitState;
use super::schema::SqliteTables;
use super::storage::{DocumentAction, address_parts, encode_indexed_string, encode_json};

const MAX_ARRAY_CHARACTERS: usize = 512 * 1024;

#[derive(Default)]
pub(super) struct PendingWrites {
    record_ids: BTreeMap<u64, &'static str>,
    conversations: Vec<Vec<SqliteValue>>,
    entries: Vec<Vec<SqliteValue>>,
    tasks: BTreeMap<u64, Vec<SqliteValue>>,
    submissions: BTreeMap<u64, Vec<SqliteValue>>,
    documents: BTreeMap<u64, DocumentRecord>,
    cleared: BTreeSet<u64>,
    revisions: BTreeMap<u64, Vec<Vec<SqliteValue>>>,
}

impl PendingWrites {
    pub(super) fn add(
        &mut self,
        writes: &[StorageWrite],
        actions: &[(DocumentId, DocumentAction)],
        seq: Seq,
        state: &CommitState,
    ) -> StorageResult<()> {
        for write in writes {
            self.add_write(write, seq)?;
        }
        for (id, action) in actions {
            let record = state
                .document(*id)
                .expect("accepted document action has a record");
            self.add_document(*id, action, seq, record)?;
        }
        Ok(())
    }

    fn add_write(&mut self, write: &StorageWrite, seq: Seq) -> StorageResult<()> {
        if let (Some(id), Some(table)) = (write.write_id(), write.table_name()) {
            self.record_ids.insert(id, table);
        }
        match write {
            StorageWrite::Conversation { value } => {
                self.conversations.push(vec![
                    value.id.get().into(),
                    value
                        .owner
                        .as_ref()
                        .map(|owner| owner.conversation_id.get() as i64)
                        .into(),
                    value
                        .owner
                        .as_ref()
                        .map(|owner| owner.task_id.get() as i64)
                        .into(),
                    encode_json(value)?.into(),
                ]);
            }
            StorageWrite::Entry { value } => {
                self.entries.push(vec![
                    value.id.get().into(),
                    value.conversation_id.get().into(),
                    value.head.map(|id| id.get()).into(),
                    seq.0.into(),
                    encode_json(value)?.into(),
                ]);
            }
            StorageWrite::Task { value } => {
                self.tasks.insert(
                    value.id.get(),
                    vec![
                        value.id.get().into(),
                        value.conversation_id.get().into(),
                        encode_indexed_string(&value.kind).into(),
                        value.status().as_str().into(),
                        value.abort_requested.into(),
                        value.background.into(),
                        encode_json(value)?.into(),
                    ],
                );
            }
            StorageWrite::Submission { value } => {
                self.submissions.insert(
                    value.id().get(),
                    vec![
                        value.id().get().into(),
                        value.conversation_id().get().into(),
                        value.request_id().map(encode_indexed_string).into(),
                        value.status().into(),
                        encode_json(value)?.into(),
                    ],
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn add_document(
        &mut self,
        id: DocumentId,
        action: &DocumentAction,
        seq: Seq,
        record: &DocumentRecord,
    ) -> StorageResult<()> {
        if action.create.is_some() || action.retire {
            self.documents.insert(id.get(), record.clone());
        }
        if let Some(content) = &action.content {
            if content.is_base() && is_current_only(record) {
                self.clear_revisions(id);
            }
            let encoded = match content {
                DocumentContent::Base { value, .. } => encode_json(value)?,
                DocumentContent::Delta { ops, .. } => encode_json(ops)?,
            };
            self.revisions.entry(id.get()).or_default().push(vec![
                id.get().into(),
                seq.0.into(),
                if content.is_base() { "base" } else { "delta" }.into(),
                content.version().into(),
                encoded.into(),
            ]);
        }
        if action.retire && is_current_only(record) {
            self.clear_revisions(id);
        }
        Ok(())
    }

    fn clear_revisions(&mut self, id: DocumentId) {
        self.cleared.insert(id.get());
        self.revisions.remove(&id.get());
    }

    pub(super) async fn flush(
        &mut self,
        tx: &SqliteTransaction,
        tables: &SqliteTables,
    ) -> StorageResult<()> {
        insert_rows(
            tx,
            &tables.record_ids,
            &["id", "record_type"],
            self.record_ids
                .iter()
                .map(|(id, table)| vec![(*id).into(), (*table).into()])
                .collect(),
            "ON CONFLICT(id) DO NOTHING",
        )
        .await?;
        insert_rows(
            tx,
            &tables.conversations,
            &["id", "owner_conversation_id", "owner_task_id", "record"],
            std::mem::take(&mut self.conversations),
            "",
        )
        .await?;
        insert_rows(
            tx,
            &tables.entries,
            &["id", "conversation_id", "head", "commit_seq", "record"],
            std::mem::take(&mut self.entries),
            "",
        )
        .await?;
        insert_rows(
            tx,
            &tables.tasks,
            &[
                "id",
                "conversation_id",
                "kind",
                "status",
                "abort_requested",
                "background",
                "record",
            ],
            std::mem::take(&mut self.tasks)
                .into_values()
                .collect(),
            "ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id, kind = excluded.kind,
             status = excluded.status, abort_requested = excluded.abort_requested,
             background = excluded.background, record = excluded.record",
        )
        .await?;
        insert_rows(
            tx,
            &tables.submissions,
            &["id", "conversation_id", "request_id", "status", "record"],
            std::mem::take(&mut self.submissions)
                .into_values()
                .collect(),
            "ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id,
             request_id = excluded.request_id, status = excluded.status, record = excluded.record",
        )
        .await?;
        let documents = std::mem::take(&mut self.documents)
            .into_values()
            .map(|record| document_row(&record))
            .collect::<StorageResult<Vec<_>>>()?;
        insert_rows(
            tx,
            &tables.documents,
            &[
                "id",
                "kind",
                "family",
                "key_value",
                "scope_kind",
                "owner_id",
                "created_at",
                "retired_at",
                "record",
            ],
            documents,
            "ON CONFLICT(id) DO UPDATE SET retired_at = excluded.retired_at, record = excluded.record",
        )
        .await?;
        run_for_each(
            tx,
            &format!(
                "DELETE FROM {} WHERE document_id IN (SELECT value FROM json_each(?))",
                tables.document_revisions
            ),
            std::mem::take(&mut self.cleared).into_iter().collect(),
        )
        .await?;
        insert_rows(
            tx,
            &tables.document_revisions,
            &["document_id", "seq", "kind", "version", "content"],
            std::mem::take(&mut self.revisions)
                .into_values()
                .flatten()
                .collect(),
            "",
        )
        .await?;
        *self = Self::default();
        Ok(())
    }
}

fn document_row(record: &DocumentRecord) -> StorageResult<Vec<SqliteValue>> {
    let parts = address_parts(&record.kind, &record.scope, record.key.as_deref());
    Ok(vec![
        record.id.get().into(),
        parts.kind.as_str().into(),
        parts.family.into(),
        parts.key_value.as_str().into(),
        parts.scope_kind.into(),
        parts.owner_id.into(),
        record.created_at.0.into(),
        record.retired_at.map(|seq| seq.0).into(),
        encode_json(record)?.into(),
    ])
}

async fn insert_rows(
    tx: &SqliteTransaction,
    table: &str,
    columns: &[&str],
    rows: Vec<Vec<SqliteValue>>,
    conflict: &str,
) -> StorageResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let cells = (0..columns.len())
        .map(|index| format!("json_extract(item.value, '$[{index}]')"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {table} ({}) SELECT {cells} FROM json_each(?) AS item WHERE true {conflict}",
        columns.join(", ")
    );
    let rows = rows
        .iter()
        .map(|row| {
            let cells = row
                .iter()
                .map(json_cell)
                .collect::<StorageResult<Vec<_>>>()?;
            Ok(serde_json::to_string(&cells)?)
        })
        .collect::<StorageResult<Vec<_>>>()?;
    for array in json_arrays(rows) {
        tx.run(&sql, vec![array.into()]).await?;
    }
    Ok(())
}

fn json_cell(value: &SqliteValue) -> StorageResult<JsonValue> {
    match value {
        SqliteValue::Null => Ok(JsonValue::Null),
        SqliteValue::Integer(value) => Ok(JsonValue::from(*value)),
        SqliteValue::Real(value) => Ok(JsonValue::from(*value)),
        SqliteValue::Text(value) => Ok(JsonValue::String(value.clone())),
        SqliteValue::Blob(_) => Err(StorageError::message(
            "SQLite commit rows cannot encode a blob through json_each",
        )),
    }
}

async fn run_for_each(tx: &SqliteTransaction, sql: &str, values: Vec<u64>) -> StorageResult<()> {
    if values.is_empty() {
        return Ok(());
    }
    let values = values
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    for array in json_arrays(values) {
        tx.run(sql, vec![array.into()]).await?;
    }
    Ok(())
}

fn json_arrays(parts: Vec<String>) -> Vec<String> {
    let mut arrays = Vec::new();
    let mut chunk = Vec::new();
    let mut length = 2usize;
    for part in parts {
        if !chunk.is_empty() && length + 1 + part.len() > MAX_ARRAY_CHARACTERS {
            arrays.push(format!("[{}]", chunk.join(",")));
            chunk = Vec::new();
            length = 2;
        }
        length += usize::from(!chunk.is_empty()) + part.len();
        chunk.push(part);
    }
    if !chunk.is_empty() {
        arrays.push(format!("[{}]", chunk.join(",")));
    }
    arrays
}
