//! Durable storage stack: port of `packages/durable` (`@earendil-works/pi-durable`
//! @ 4a080252f / v1.4.0) — the M4 storage foundation.
//!
//! **Experimental.** The API changes without notice between releases.
//!
//! One process owns a storage at a time; there is no cross-process locking. The
//! crate carries the portable storage contract ([`types::Storage`]), the Memory,
//! JSONL and SQLite backends, the asynchronous SQLite facade and schema
//! identifiers, and the consumed subset of `@earendil-works/chord`
//! (context/json/delta/state and the portable `/sqlite` drivers).
//!
//! The SQLite backend migrates a version 1 database to schema version 2
//! (the `sleeping` task status) when it first opens it. A migrated database
//! cannot be opened by an earlier rpi version; JSONL needs no migration. Pass
//! [`storage::SqliteStorageOptions::prefix`] to keep every durable table and
//! index under a namespace in a shared database.
//!
//! This crate is the storage layer of the durable band. It does not participate
//! in the default CLI path: `crates/rpi` does not depend on `rpi-durable`.

pub mod chord;
pub mod codemode;
pub mod documents;
pub mod entries;
pub mod env;
pub mod errors;
pub mod fs;
pub mod harness;
pub mod ids;
pub mod search;
pub mod session;
pub mod storage;
pub mod testing;
pub mod timers;
pub mod tool_search;
pub mod tools;
pub mod truncate;
pub mod types;

pub use chord::sqlite::validate_sqlite_schema_prefix;
pub use errors::{FileError, FileErrorCode, StorageError, StorageResult};
pub use ids::{
    ConversationId, DocumentId, EntryId, Id, ROOT_CONVERSATION_ID, Seq, SubmissionId, TaskId,
    id_from_number, seq_from_number,
};
pub use storage::{
    CURRENT_SQLITE_SCHEMA_VERSION, DEFAULT_SQLITE_SCHEMA, DirEntry, DirEntryKind, FileSystem,
    JsonlStorage, JsonlStorageOptions, MemoryStorage, NodeSqliteStorageOptions,
    PreparedMemoryCommit, SqliteDatabase, SqliteDatabaseOptions, SqliteExecutor, SqliteIndexes,
    SqliteMigration, SqliteSchema, SqliteStorage, SqliteStorageOptions, SqliteTables,
    SqliteTransaction, SqliteValue, StdFileSystem, apply_sqlite_migrations,
    apply_sqlite_migrations_for_schema, create_sqlite_migrations, create_sqlite_schema,
    open_node_sqlite_database, open_node_sqlite_storage, sqlite_migrations,
};
pub use types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent,
    DocumentCreate, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope, EntryQuery,
    EntryRecord, Page, ScanOrder, Storage, StorageExt, StorageWrite, StoredDocument, StoredEntry,
    SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord, TaskState,
};

pub use chord::context::{BACKGROUND_CONTEXT, Context, ContextKey, TODO_CONTEXT};
pub use chord::json::{JsonObject, JsonValue, copy_json, is_json_value};
pub use chord::tracker::{Change, DocCell, Prepared, Tracker, track};
pub use documents::{
    DocDefinition, DocScope, DocTarget, DocToken, DocumentError, ResolvedAddress, address_id,
    check_record_scope, check_record_version, define_doc, define_doc_family, document_create,
    materialize_document, materialize_document_value, resolve_address,
};
pub use session::{
    CloseListener, CommitChange, CommitFn, CommitListener, CommitPublication, CommittedStateSource,
    CommittedWatch, ConversationCreatedHook, ConversationDocumentView, ConversationOwnership,
    DocDraft, DocOwner, DocumentChange, DocumentCopyChange, EntryDraft, EntryHead,
    ForkDocumentCopy, LoadedDocument, NowFn, ObservedDocumentValue, SessionError, SessionFuture,
    SessionImpl, SessionResult, Subscription, TaskCreate, TaskOwnership, Transaction,
    TransactionScope, WatchEnd, WatchListener, commit_fn, create_session,
    prepare_fork_document_copies, watch_listener,
};
