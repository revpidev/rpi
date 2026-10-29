//! Consumed subset of `@earendil-works/chord/sqlite` (port of
//! `packages/chord/src/sqlite` @ `4a080252f`).
//!
//! Carries the portable SQLite driver interfaces ([`database`]), the generic
//! migration runner and schema-prefix validation. Durable storage code and its
//! historical entry points (`rpi_durable::storage::sqlite::*`) re-export these
//! names, so existing imports keep working.

pub mod database;
pub mod migrations;
pub mod node;
pub mod schema;

pub use database::{
    DEFAULT_BUSY_TIMEOUT_MS, DEFAULT_WAL_AUTO_CHECKPOINT_PAGES, SqliteDatabase,
    SqliteDatabaseOptions, SqliteExecutor, SqliteTransaction, SqliteValue, SqliteValueRef,
};
pub use migrations::{SqliteMigration, apply_sqlite_migrations};
pub use node::open_node_sqlite_database;
pub use schema::validate_sqlite_schema_prefix;
