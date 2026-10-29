//! Generic SQLite migration runner (port of `src/sqlite/migrations.ts` @
//! `4a080252f`).
//!
//! The runner validates a prefix-safe metadata table, checks contiguous
//! versions and applies every pending migration in one transaction. Durable
//! schema history lives in [`crate::storage::sqlite::migrations`].

use crate::chord::sqlite::database::{SqliteDatabase, SqliteValue};
use crate::chord::sqlite::schema::validate_sqlite_schema_prefix;
use crate::errors::{StorageError, StorageResult};

/// One schema migration step.
#[derive(Debug, Clone)]
pub struct SqliteMigration {
    /// Contiguous version starting at 1.
    pub version: u64,
    /// Statements applied in order inside the migration transaction.
    pub statements: Vec<String>,
}

/// Apply all pending migrations atomically, tracking their version in `table`.
pub async fn apply_sqlite_migrations(
    db: &SqliteDatabase,
    migrations: &[SqliteMigration],
    table: &str,
) -> StorageResult<()> {
    validate_sqlite_schema_prefix(table)?;
    for (index, migration) in migrations.iter().enumerate() {
        if migration.version != index as u64 + 1 {
            return Err(StorageError::message(
                "SQLite migrations must have contiguous versions starting at 1",
            ));
        }
    }
    let current_version = migrations
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);
    let create = format!(
        r#"CREATE TABLE IF NOT EXISTS {table} (
			singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
			version INTEGER NOT NULL CHECK (version >= 0)
		) STRICT"#
    );
    let insert = format!("INSERT OR IGNORE INTO {table} (singleton, version) VALUES (1, 0)");
    let select = format!("SELECT version FROM {table} WHERE singleton = 1");
    let update = format!("UPDATE {table} SET version = ? WHERE singleton = 1");
    db.transaction(move |tx| async move {
        tx.exec(&create).await?;
        tx.run(&insert, vec![]).await?;
        let version = tx.get_i64(&select, vec![]).await?;
        let Some(version) = version else {
            return Err(StorageError::message("SQLite schema metadata is missing"));
        };
        let version = version as u64;
        if version > current_version {
            return Err(StorageError::message(format!(
                "SQLite schema version {version} is newer than supported version {current_version}"
            )));
        }
        for migration in migrations {
            if migration.version <= version {
                continue;
            }
            for statement in &migration.statements {
                tx.exec(statement).await?;
            }
            tx.run(
                &update,
                vec![SqliteValue::Integer(migration.version as i64)],
            )
            .await?;
        }
        Ok(())
    })
    .await
}
