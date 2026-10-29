//! Immutable, ordered durable SQLite schema history
//! (port of `src/storage/sqlite/migrations.ts` @ `4a080252f`).
//!
//! Version 2 adds the `sleeping` task status. SQLite cannot change a `CHECK`
//! constraint in place, so the task table is rebuilt and its rows and indexes
//! are carried over. The migration history is immutable: v1 must keep rendering
//! the original objects for databases that still report version 1.

use crate::chord::sqlite::database::SqliteDatabase;
use crate::chord::sqlite::migrations::apply_sqlite_migrations as apply_chord_migrations;
use crate::errors::StorageResult;
use crate::storage::sqlite::schema::{DEFAULT_SQLITE_SCHEMA, SqliteSchema};

pub use crate::chord::sqlite::migrations::SqliteMigration;

// next_id is TEXT because SQLite bindings reject INTEGER values outside the
// JavaScript safe integer range.
fn initial_schema(schema: &SqliteSchema) -> Vec<String> {
    let t = &schema.tables;
    let i = &schema.indexes;
    vec![
        format!(
            r#"CREATE TABLE {} (
		singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
		next_id TEXT NOT NULL,
		next_seq INTEGER NOT NULL
	) STRICT"#,
            t.durable_metadata
        ),
        format!(
            "INSERT INTO {} (singleton, next_id, next_seq) VALUES (1, '2', 1)",
            t.durable_metadata
        ),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		record_type TEXT NOT NULL CHECK (record_type IN ('conversation', 'entry', 'task', 'submission', 'document'))
	) STRICT"#,
            t.record_ids
        ),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		owner_conversation_id INTEGER,
		owner_task_id INTEGER,
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.conversations
        ),
        format!(
            "CREATE INDEX {} ON {} (owner_conversation_id, id)",
            i.conversations_by_owner_conversation, t.conversations
        ),
        format!(
            "CREATE INDEX {} ON {} (owner_task_id, id)",
            i.conversations_by_owner_task, t.conversations
        ),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		conversation_id INTEGER NOT NULL,
		head INTEGER,
		commit_seq INTEGER NOT NULL,
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.entries
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id DESC)",
            i.entries_by_conversation, t.entries
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id DESC) WHERE head IS NOT NULL",
            i.entry_heads_by_conversation, t.entries
        ),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		conversation_id INTEGER NOT NULL,
		kind TEXT NOT NULL,
		status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'completing', 'terminal')),
		abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
		background INTEGER NOT NULL CHECK (background IN (0, 1)),
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.tasks
        ),
        format!(
            "CREATE INDEX {} ON {} (status, id)",
            i.tasks_by_status, t.tasks
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id)",
            i.tasks_by_conversation, t.tasks
        ),
        format!("CREATE INDEX {} ON {} (kind, id)", i.tasks_by_kind, t.tasks),
        format!(
            "CREATE INDEX {} ON {} (abort_requested, id)",
            i.tasks_by_abort_requested, t.tasks
        ),
        format!(
            "CREATE INDEX {} ON {} (background, id)",
            i.tasks_by_background, t.tasks
        ),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		conversation_id INTEGER NOT NULL,
		request_id TEXT,
		status TEXT NOT NULL CHECK (status IN ('queued', 'placed', 'done', 'unanswered')),
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.submissions
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, request_id)",
            i.submissions_by_request, t.submissions
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id)",
            i.submissions_by_conversation, t.submissions
        ),
        format!(
            "CREATE INDEX {} ON {} (status, id)",
            i.submissions_by_status, t.submissions
        ),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		kind TEXT NOT NULL,
		family INTEGER NOT NULL CHECK (family IN (0, 1)),
		key_value TEXT NOT NULL,
		scope_kind TEXT NOT NULL CHECK (scope_kind IN ('session', 'conversation', 'task')),
		owner_id INTEGER NOT NULL,
		created_at INTEGER NOT NULL,
		retired_at INTEGER,
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.documents
        ),
        format!(
            r#"CREATE INDEX {}
		ON {} (kind, scope_kind, owner_id, family, key_value, created_at DESC, retired_at)"#,
            i.documents_by_address, t.documents
        ),
        format!(
            "CREATE INDEX {} ON {} (scope_kind, owner_id, id)",
            i.documents_by_scope, t.documents
        ),
        format!(
            "CREATE INDEX {} ON {} (scope_kind, owner_id, kind, id)",
            i.documents_by_scope_kind, t.documents
        ),
        format!(
            r#"CREATE TABLE {} (
		document_id INTEGER NOT NULL,
		seq INTEGER NOT NULL,
		kind TEXT NOT NULL CHECK (kind IN ('base', 'delta')),
		version INTEGER NOT NULL,
		content TEXT NOT NULL CHECK (json_valid(content)),
		PRIMARY KEY (document_id, seq)
	) STRICT"#,
            t.document_revisions
        ),
        format!(
            "CREATE INDEX {} ON {} (document_id, kind, seq DESC)",
            i.document_revisions_by_kind, t.document_revisions
        ),
    ]
}

/// Version 2 adds the `sleeping` task status by rebuilding the task table
/// (upstream v1.4.0 `sleepingStatus`).
fn sleeping_status(schema: &SqliteSchema) -> Vec<String> {
    let t = &schema.tables;
    let i = &schema.indexes;
    let rebuilt = format!("{}_v2", t.tasks);
    vec![
        format!(
            r#"CREATE TABLE {rebuilt} (
			id INTEGER PRIMARY KEY,
			conversation_id INTEGER NOT NULL,
			kind TEXT NOT NULL,
			status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'sleeping', 'completing', 'terminal')),
			abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
			background INTEGER NOT NULL CHECK (background IN (0, 1)),
			record TEXT NOT NULL CHECK (json_valid(record))
		) STRICT"#
        ),
        format!(
            "INSERT INTO {rebuilt} (id, conversation_id, kind, status, abort_requested, background, record)
			SELECT id, conversation_id, kind, status, abort_requested, background, record FROM {}",
            t.tasks
        ),
        format!("DROP TABLE {}", t.tasks),
        format!("ALTER TABLE {rebuilt} RENAME TO {}", t.tasks),
        format!(
            "CREATE INDEX {} ON {} (status, id)",
            i.tasks_by_status, t.tasks
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id)",
            i.tasks_by_conversation, t.tasks
        ),
        format!(
            "CREATE INDEX {} ON {} (kind, id)",
            i.tasks_by_kind, t.tasks
        ),
        format!(
            "CREATE INDEX {} ON {} (abort_requested, id)",
            i.tasks_by_abort_requested, t.tasks
        ),
        format!(
            "CREATE INDEX {} ON {} (background, id)",
            i.tasks_by_background, t.tasks
        ),
    ]
}

/// Build the immutable migration history for one set of SQLite identifiers.
pub fn create_sqlite_migrations(schema: &SqliteSchema) -> Vec<SqliteMigration> {
    vec![
        SqliteMigration {
            version: 1,
            statements: initial_schema(schema),
        },
        SqliteMigration {
            version: 2,
            statements: sleeping_status(schema),
        },
    ]
}

/// Latest schema version supported by this build.
pub const CURRENT_SQLITE_SCHEMA_VERSION: u64 = 2;

/// Immutable, ordered schema history for the default (unprefixed) schema.
pub fn sqlite_migrations() -> Vec<SqliteMigration> {
    create_sqlite_migrations(&DEFAULT_SQLITE_SCHEMA)
}

/// Apply pending migrations to the default durable schema.
pub async fn apply_sqlite_migrations(
    db: &SqliteDatabase,
    migrations: &[SqliteMigration],
) -> StorageResult<()> {
    apply_sqlite_migrations_for_schema(db, &DEFAULT_SQLITE_SCHEMA, migrations).await
}

/// Apply pending migrations to the identifiers rendered for `schema`.
pub async fn apply_sqlite_migrations_for_schema(
    db: &SqliteDatabase,
    schema: &SqliteSchema,
    migrations: &[SqliteMigration],
) -> StorageResult<()> {
    apply_chord_migrations(db, migrations, &schema.tables.durable_schema).await
}

#[cfg(test)]
mod tests {
    use super::{CURRENT_SQLITE_SCHEMA_VERSION, sqlite_migrations};

    #[test]
    fn the_latest_migration_matches_the_supported_version() {
        assert_eq!(
            sqlite_migrations()
                .last()
                .map(|migration| migration.version),
            Some(CURRENT_SQLITE_SCHEMA_VERSION)
        );
    }

    /// The fixture was extracted verbatim from
    /// `543e206:crates/rpi-durable/src/storage/sqlite/migrations.rs`
    /// (`initial_schema()` in v0.1.7). It is an external anchor: if the v1
    /// renderer changes, this comparison fails even though the test itself
    /// derives the current history from the same code.
    #[test]
    fn version_one_history_matches_the_v017_fixture() {
        let fixture: Vec<String> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/sqlite-v1-schema.json"
        ))
        .expect("v1 schema fixture");
        assert_eq!(
            sqlite_migrations().remove(0).statements,
            fixture,
            "the v1 history is rendered from the schema but must keep its exact v0.1.7 bytes"
        );
    }

    #[test]
    fn version_two_rebuilds_the_task_table_with_the_sleeping_status() {
        let migrations = sqlite_migrations();
        let v2 = &migrations[1].statements;
        assert!(v2[0].contains("'sleeping'"));
        assert!(v2[1].contains("INSERT INTO tasks_v2"));
        assert_eq!(
            migrations[0]
                .statements
                .iter()
                .filter(|statement| statement.contains("'sleeping'"))
                .count(),
            0,
            "the v1 history must stay immutable"
        );
    }
}
