//! Node-adapter equivalents for the portable SQLite driver (port of
//! `packages/chord/src/sqlite/node.ts` @ `4a080252f`).

use crate::chord::sqlite::database::{SqliteDatabase, SqliteDatabaseOptions};
use crate::errors::StorageResult;

/// Open and configure a file-backed database facade.
pub async fn open_node_sqlite_database(
    path: &str,
    options: SqliteDatabaseOptions,
) -> StorageResult<SqliteDatabase> {
    SqliteDatabase::open(path, options).await
}
