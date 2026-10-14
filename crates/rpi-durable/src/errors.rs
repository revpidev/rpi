//! Storage error surface (port of `src/errors.ts` + the validation errors the
//! storage backends raise; upstream uses plain `Error` subclasses with the same
//! messages).

use thiserror::Error;

use crate::ids::{ConversationId, Id};

/// Failure modes raised by [`crate::storage::Storage`] implementations.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Storage rejected a batch before any durable effect (upstream `StorageRejected`).
    #[error("{0}")]
    Rejected(String),

    /// Generic validation failure; the message mirrors the upstream `Error`.
    #[error("{0}")]
    Message(String),

    /// Converation lookup in a fork-aware scan.
    #[error("Unknown conversation: {0}")]
    UnknownConversation(ConversationId),

    /// A cursor whose `after`/`order` fields are not a valid storage cursor.
    #[error("Invalid storage cursor")]
    InvalidCursor,

    /// A cursor continues one order while the query asks for the other.
    #[error("The cursor continues a {stored} scan; the query asks for {requested}")]
    CursorOrder { stored: String, requested: String },

    /// A closed backend rejects every later operation.
    #[error("{0} is closed")]
    Closed(&'static str),

    /// Minting would leave the safe integer ID space.
    #[error("ID space is exhausted")]
    IdSpaceExhausted,

    /// `Tx` table reads after the first table write (upstream `ReadAfterWrite`).
    #[error("Tx.{0}() cannot read tables after the first table write")]
    ReadAfterWrite(&'static str),

    /// A submission reached a busy conversation.
    #[error("Conversation {0} is busy")]
    ConversationBusy(ConversationId),

    /// Corrupt complete JSONL line or marker.
    #[error("{0}")]
    JsonlCorruption(String),

    /// A JSONL storage whose write path failed and which must be reopened.
    #[error("JSONL storage is poisoned and must be reopened")]
    JsonlPoisoned(String),

    /// JSON encode/decode failure.
    #[error("{0}")]
    Json(#[from] serde_json::Error),

    /// Filesystem failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// SQLite failure.
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Delta operation failure.
    #[error("{0}")]
    Delta(#[from] crate::chord::delta::DeltaError),

    /// Operation on a settled SQLite transaction handle.
    #[error("SQLite transaction handle is no longer active")]
    TransactionInactive,

    /// The SQLite facade was used from inside its own transaction callback,
    /// where upstream never settles. Replaced with an explicit error (form
    /// difference, V17-13 §7.3).
    #[error("SQLite database cannot be used from inside its own transaction callback")]
    TransactionReentrant,

    /// SQLite rollback failed after the callback failed; the callback error is
    /// not a guaranteed rollback (upstream `AggregateError`).
    #[error("SQLite transaction failed and rollback failed")]
    RollbackFailed {
        /// Original callback failure.
        #[source]
        callback: Box<StorageError>,
        /// Rollback failure.
        rollback: Box<StorageError>,
    },

    /// A filesystem seam failure carrying the upstream `FileError` code.
    #[error("File operation failed: {0}")]
    File(#[from] FileError),
}

impl StorageError {
    /// Build a generic validation error with the upstream message text.
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }

    /// Build an `ID {id} already belongs to {table}` error.
    pub fn id_belongs_to<T>(id: Id<T>, table: &'static str) -> Self {
        Self::Message(format!("ID {id} already belongs to {table}"))
    }
}

/// Filesystem seam error (port of the result-shaped `FileError` from `env/`).
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct FileError {
    /// Upstream `FileError.code`; `not_found` is control flow in JSONL recovery.
    pub code: FileErrorCode,
    /// Human-readable message.
    pub message: String,
}

impl FileError {
    pub fn not_found(path: impl Into<String>) -> Self {
        Self {
            code: FileErrorCode::NotFound,
            message: format!("File not found: {}", path.into()),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self {
            code: FileErrorCode::Other,
            message: message.into(),
        }
    }
}

/// Upstream `FileError` code subset used by the storage stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErrorCode {
    /// The path does not exist.
    NotFound,
    /// Any other filesystem failure.
    Other,
}

/// Upstream `StorageRejected` marker used by error-matching tests.
pub type StorageRejected = StorageError;
/// Convenience result alias.
pub type StorageResult<T> = Result<T, StorageError>;
