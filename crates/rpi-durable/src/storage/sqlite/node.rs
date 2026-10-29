//! Node-adapter equivalents: open a file-backed database or storage with the
//! upstream defaults (port of `packages/chord/src/sqlite/node.ts` and
//! `src/storage/sqlite/node.ts` @ `4a080252f`).

use crate::chord::sqlite::database::SqliteDatabaseOptions;
use crate::chord::sqlite::validate_sqlite_schema_prefix;
use crate::errors::StorageResult;
use crate::storage::sqlite::storage::{SqliteStorage, SqliteStorageOptions};

pub use crate::chord::sqlite::node::open_node_sqlite_database;

/// Node SQLite connection settings plus durable storage options
/// (upstream `NodeSqliteStorageOptions`).
#[derive(Debug, Clone, Default)]
pub struct NodeSqliteStorageOptions {
    /// SQLite WAL auto-checkpoint threshold; `Some(0)` disables it. Default 1000 pages.
    pub wal_auto_checkpoint_pages: Option<i64>,
    /// Time SQLite waits for a competing file lock. Default 5000 ms.
    pub busy_timeout_ms: Option<u64>,
    /// Prefix for every durable table and index in the database.
    pub prefix: Option<String>,
}

/// Open or create file-backed durable storage, optionally under a table prefix.
pub async fn open_node_sqlite_storage(
    path: &str,
    options: NodeSqliteStorageOptions,
) -> StorageResult<SqliteStorage> {
    // Validate before creating the database file, like upstream node.ts. The
    // storage open repeats the check for callers that build it directly.
    if let Some(prefix) = &options.prefix {
        validate_sqlite_schema_prefix(prefix)?;
    }
    let database_options = SqliteDatabaseOptions {
        wal_auto_checkpoint_pages: options.wal_auto_checkpoint_pages,
        busy_timeout_ms: options.busy_timeout_ms,
    };
    let storage_options = SqliteStorageOptions {
        prefix: options.prefix,
    };
    SqliteStorage::open_with_options(
        crate::chord::sqlite::node::open_node_sqlite_database(path, database_options).await?,
        storage_options,
    )
    .await
}
