//! Read/write keys for unpublished Session commits (port of
//! `packages/durable/src/session/keys.ts` @ `b4afdddf`).

use crate::ids::{ConversationId, DocumentId, EntryId, SubmissionId, TaskId};
use crate::types::{DocumentScope, StorageWrite};

pub(crate) const ALL_KEYS: &str = "*";

pub(crate) fn task_key(id: TaskId) -> String {
    format!("task:{id}")
}

pub(crate) fn conversation_key(id: ConversationId) -> String {
    format!("conv:{id}")
}

pub(crate) fn entry_key(id: EntryId) -> String {
    format!("entry:{id}")
}

pub(crate) fn append_key(id: ConversationId) -> String {
    format!("entry-append:{id}")
}

pub(crate) fn submission_key(id: SubmissionId) -> String {
    format!("sub:{id}")
}

pub(crate) fn request_key(conversation_id: ConversationId, request_id: &str) -> String {
    format!("req:{conversation_id}:{request_id}")
}

pub(crate) fn document_key(address_id: &str) -> String {
    format!("doc:{address_id}")
}

pub(crate) fn incarnation_key(id: DocumentId) -> String {
    format!("docid:{id}")
}

pub(crate) fn scope_key(scope: &DocumentScope) -> String {
    match scope {
        DocumentScope::Session => "docscope:session".to_string(),
        DocumentScope::Conversation { conversation_id } => {
            format!("docscope:c{conversation_id}")
        }
        DocumentScope::Task { task_id } => format!("docscope:t{task_id}"),
    }
}

pub(crate) fn table_keys(writes: &[StorageWrite], keys: &mut Vec<String>) {
    for write in writes {
        match write {
            StorageWrite::Conversation { value } => {
                keys.push(conversation_key(value.id));
            }
            StorageWrite::Entry { value } => {
                keys.push(entry_key(value.id));
                keys.push(append_key(value.conversation_id));
            }
            StorageWrite::Task { value } => {
                keys.push(task_key(value.id));
            }
            StorageWrite::Submission { value } => {
                keys.push(submission_key(value.id()));
                if let Some(request_id) = value.request_id() {
                    keys.push(request_key(value.conversation_id(), request_id));
                }
            }
            StorageWrite::DocumentCreate { .. }
            | StorageWrite::DocumentCopy { .. }
            | StorageWrite::DocumentChange { .. }
            | StorageWrite::DocumentRetire { .. } => {}
        }
    }
}
