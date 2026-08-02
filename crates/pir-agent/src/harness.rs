//! Harness layer — port of the `SessionStorage`-related types of
//! `packages/agent/src/harness/types.ts` @ pi 0.82.1 (2efa728).
//!
//! **Trait shape only** (T07): the JSONL/memory storage implementations and
//! the `SessionRepo` layer land in T16 (ADR-0003 §1). The trait is kept
//! isomorphic with upstream so that the T16 implementations can be validated
//! against the pinned harness code 1:1.
//!
//! Intentional differences:
//! - Upstream `SessionStorage<TMetadata>`'s generic metadata becomes an
//!   associated type (Rust idiom).
//! - `findEntries<TType>`'s type-parameter extraction has no Rust equivalent;
//!   it takes the `entry.type` tag string and returns the unified
//!   [`SessionEntry`] union (callers match on variants).
//! - Upstream `Promise` rejections become `Result<_, SessionError>`; the
//!   `SessionError.code` strings match upstream exactly.

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::session::SessionEntry;

/// `SessionStats` (harness/types.ts:473-479).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    pub message_count: u64,
    pub cached_tokens: u64,
    pub uncached_tokens: u64,
    pub total_tokens: u64,
    pub cost_total: f64,
}

/// `SessionMetadata` (harness/types.ts:481-484).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMetadata {
    pub id: String,
    pub created_at: String,
}

/// `JsonlSessionMetadata` (harness/types.ts:486-491).
#[derive(Debug, Clone, PartialEq)]
pub struct JsonlSessionMetadata {
    pub base: SessionMetadata,
    pub cwd: String,
    pub path: String,
    pub parent_session_path: Option<String>,
    pub metadata: Option<Map<String, Value>>,
}

/// `SessionEntryCursorOptions` (harness/types.ts:493-496).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionEntryCursorOptions {
    pub after_entry_seq: Option<usize>,
    pub limit: Option<usize>,
}

/// `SessionCreateOptions` (harness/types.ts:518-520).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionCreateOptions {
    pub id: Option<String>,
}

/// `JsonlSessionCreateOptions` (harness/types.ts:540-544).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JsonlSessionCreateOptions {
    pub base: SessionCreateOptions,
    pub cwd: String,
    pub parent_session_path: Option<String>,
    pub metadata: Option<Map<String, Value>>,
}

/// `SessionForkOptions["position"]` (harness/types.ts:524).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForkPosition {
    /// Fork *before* the target entry; the target must be a user message
    /// (repo-utils.ts:42-48).
    #[default]
    Before,
    /// Fork *at* the target entry (included).
    At,
}

/// `SessionForkOptions` (harness/types.ts:522-526).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionForkOptions {
    pub entry_id: Option<String>,
    pub position: Option<ForkPosition>,
    pub id: Option<String>,
}

/// `SessionError` (harness/types.ts): `code` strings match upstream exactly
/// (`not_found` / `storage` / `invalid_session` / `invalid_entry` /
/// `invalid_fork_target`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("session error ({code}): {message}")]
pub struct SessionError {
    pub code: &'static str,
    pub message: String,
}

impl SessionError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        SessionError {
            code,
            message: message.into(),
        }
    }
}

/// `SessionStorage` (harness/types.ts:498-514) — storage backend contract for
/// the harness session tree. Implementations land in T16
/// (`jsonl-storage.ts` / `memory-storage.ts` ports).
#[async_trait]
pub trait SessionStorage: Send + Sync {
    /// Upstream `TMetadata` generic parameter.
    type Metadata;

    /// `getMetadata`.
    async fn get_metadata(&self) -> Result<Self::Metadata, SessionError>;

    /// `getLeafId`.
    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError>;

    /// `setLeafId` — persist a leaf entry recording the active tree leaf.
    async fn set_leaf_id(&mut self, leaf_id: Option<String>) -> Result<(), SessionError>;

    /// `createEntryId`.
    async fn create_entry_id(&self) -> Result<String, SessionError>;

    /// `appendEntry`.
    async fn append_entry(&mut self, entry: SessionEntry) -> Result<(), SessionError>;

    /// `getEntry`.
    async fn get_entry(&self, id: &str) -> Result<Option<SessionEntry>, SessionError>;

    /// `findEntries<TType>` — filter by `entry.type` tag (see header note).
    async fn find_entries(&self, entry_type: &str) -> Result<Vec<SessionEntry>, SessionError>;

    /// `getLabel`.
    async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError>;

    /// `getSessionName`.
    async fn get_session_name(&self) -> Result<Option<String>, SessionError>;

    /// `getSessionStats`.
    async fn get_session_stats(&self) -> Result<SessionStats, SessionError>;

    /// `getPathToRootOrCompaction`.
    async fn get_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionEntry>, SessionError>;

    /// `getEntries`.
    async fn get_entries(
        &self,
        options: SessionEntryCursorOptions,
    ) -> Result<Vec<SessionEntry>, SessionError>;
}
