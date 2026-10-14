//! Durable storage stack: port of `packages/durable` (`@earendil-works/pi-durable`
//! @ abe508e1b / v1.1.0) — the M4 storage foundation.
//!
//! **Experimental.** The API changes without notice between releases.
//!
//! One process owns a storage at a time; there is no cross-process locking. The
//! crate carries the portable storage contract ([`types::Storage`]), the Memory,
//! JSONL and SQLite backends, the asynchronous SQLite facade, and the consumed
//! subset of `@earendil-works/chord` (context/json/delta/state).
//!
//! This crate is the storage layer of the durable band. It does not participate
//! in the default CLI path: `crates/rpi` does not depend on `rpi-durable`.

pub mod chord;
pub mod errors;
pub mod fs;
pub mod ids;
pub mod storage;
pub mod testing;
pub mod types;

pub use errors::{FileError, FileErrorCode, StorageError, StorageResult};
pub use ids::{
    ConversationId, DocumentId, EntryId, Id, ROOT_CONVERSATION_ID, Seq, SubmissionId, TaskId,
    id_from_number, seq_from_number,
};
pub use storage::{
    CURRENT_SQLITE_SCHEMA_VERSION, DirEntry, DirEntryKind, FileSystem, JsonlStorage,
    JsonlStorageOptions, MemoryStorage, PreparedMemoryCommit, SqliteDatabase,
    SqliteDatabaseOptions, SqliteExecutor, SqliteMigration, SqliteStorage, SqliteTransaction,
    SqliteValue, StdFileSystem, apply_sqlite_migrations, open_node_sqlite_database,
    open_node_sqlite_storage, sqlite_migrations,
};
pub use types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent,
    DocumentCreate, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope, EntryQuery,
    EntryRecord, Page, ScanOrder, Storage, StorageExt, StorageWrite, StoredDocument, StoredEntry,
    SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord, TaskState,
};

pub use chord::context::{BACKGROUND_CONTEXT, Context, ContextKey, TODO_CONTEXT};
pub use chord::json::{JsonObject, JsonValue, copy_json, is_json_value};
