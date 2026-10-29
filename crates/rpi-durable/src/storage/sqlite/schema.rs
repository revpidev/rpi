//! Durable SQLite identifiers (port of `src/storage/sqlite/schema.ts` @
//! `4a080252f`).
//!
//! Every table and index can be moved under a caller prefix so durable storage
//! can share a database with host tables. The identifier history is immutable:
//! a rename adds a new entry so historical migrations keep rendering the
//! original object names.

use std::sync::LazyLock;

pub use crate::chord::sqlite::validate_sqlite_schema_prefix;

use crate::errors::StorageResult;

/// Define one identifier group with its textual SQLite names.
macro_rules! schema_identifiers {
    (
        $(#[$type_doc:meta])*
        pub struct $group:ident {
            $(
                $field:ident = $name:literal,
            )+
        }
    ) => {
        $(#[$type_doc])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $group {
            $(
                pub $field: String,
            )+
        }

        impl $group {
            fn render(prefix: &str) -> Self {
                Self {
                    $(
                        $field: format!("{prefix}{}", $name),
                    )+
                }
            }
        }
    };
}

schema_identifiers! {
    /// Table identifiers used by migrations and runtime queries.
    pub struct SqliteTables {
        durable_schema = "durable_schema",
        durable_metadata = "durable_metadata",
        record_ids = "record_ids",
        conversations = "conversations",
        entries = "entries",
        tasks = "tasks",
        submissions = "submissions",
        documents = "documents",
        document_revisions = "document_revisions",
    }
}

schema_identifiers! {
    /// Index identifiers used by migrations and runtime queries.
    pub struct SqliteIndexes {
        conversations_by_owner_conversation = "conversations_by_owner_conversation",
        conversations_by_owner_task = "conversations_by_owner_task",
        entries_by_conversation = "entries_by_conversation",
        entry_heads_by_conversation = "entry_heads_by_conversation",
        tasks_by_status = "tasks_by_status",
        tasks_by_conversation = "tasks_by_conversation",
        tasks_by_kind = "tasks_by_kind",
        tasks_by_abort_requested = "tasks_by_abort_requested",
        tasks_by_background = "tasks_by_background",
        submissions_by_request = "submissions_by_request",
        submissions_by_conversation = "submissions_by_conversation",
        submissions_by_status = "submissions_by_status",
        documents_by_address = "documents_by_address",
        documents_by_scope = "documents_by_scope",
        documents_by_scope_kind = "documents_by_scope_kind",
        document_revisions_by_kind = "document_revisions_by_kind",
    }
}

/// Identifiers for one durable schema prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteSchema {
    pub tables: SqliteTables,
    pub indexes: SqliteIndexes,
}

impl SqliteSchema {
    fn render(prefix: &str) -> Self {
        Self {
            tables: SqliteTables::render(prefix),
            indexes: SqliteIndexes::render(prefix),
        }
    }
}

/// Build the identifiers used by every migration and storage query.
pub fn create_sqlite_schema(prefix: Option<&str>) -> StorageResult<SqliteSchema> {
    if let Some(prefix) = prefix {
        validate_sqlite_schema_prefix(prefix)?;
    }
    Ok(SqliteSchema::render(prefix.unwrap_or("")))
}

/// Default unprefixed identifiers.
pub static DEFAULT_SQLITE_SCHEMA: LazyLock<SqliteSchema> =
    LazyLock::new(|| SqliteSchema::render(""));

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SQLITE_SCHEMA, create_sqlite_schema};

    #[test]
    fn default_schema_matches_the_unprefixed_builder() {
        assert_eq!(*DEFAULT_SQLITE_SCHEMA, create_sqlite_schema(None).unwrap());
    }

    #[test]
    fn a_prefix_reaches_every_table_and_index() {
        let schema = create_sqlite_schema(Some("rpi_")).unwrap();
        assert_eq!(schema.tables.durable_schema, "rpi_durable_schema");
        assert_eq!(schema.tables.tasks, "rpi_tasks");
        assert_eq!(
            schema.indexes.document_revisions_by_kind,
            "rpi_document_revisions_by_kind"
        );
        assert!(
            [
                &schema.tables.durable_metadata,
                &schema.tables.record_ids,
                &schema.tables.conversations,
                &schema.tables.entries,
                &schema.tables.submissions,
                &schema.tables.documents,
                &schema.tables.document_revisions,
                &schema.indexes.conversations_by_owner_conversation,
                &schema.indexes.conversations_by_owner_task,
                &schema.indexes.entries_by_conversation,
                &schema.indexes.entry_heads_by_conversation,
                &schema.indexes.tasks_by_status,
                &schema.indexes.tasks_by_conversation,
                &schema.indexes.tasks_by_kind,
                &schema.indexes.tasks_by_abort_requested,
                &schema.indexes.tasks_by_background,
                &schema.indexes.submissions_by_request,
                &schema.indexes.submissions_by_conversation,
                &schema.indexes.submissions_by_status,
                &schema.indexes.documents_by_address,
                &schema.indexes.documents_by_scope,
                &schema.indexes.documents_by_scope_kind,
            ]
            .iter()
            .all(|name| name.starts_with("rpi_") && name.len() > 4)
        );
    }

    #[test]
    fn invalid_and_reserved_prefixes_fail_before_rendering() {
        for prefix in ["", "bad-prefix", "sqlite_x"] {
            assert!(create_sqlite_schema(Some(prefix)).is_err(), "{prefix}");
        }
    }
}
