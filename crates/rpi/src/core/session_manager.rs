//! Port of `packages/coding-agent/src/core/session-manager.ts` @ pi 0.82.1 (2efa728).
//!
//! JSONL session storage: append-only entry tree (`id`/`parentId`), v1→v3 load
//! migrations with full-file rewrite, deferred persistence (`flushed` + `wx`
//! exclusive create), compaction-aware context building, branching/forking.
//! No file locking — append-write directly, exactly like upstream (G4 red
//! line; locks are only for auth/settings/trust).
//!
//! Session format benchmark: `docs/session-format.md` (byte-level).
//!
//! Intentional differences (structural, wire-compatible):
//! - Upstream entries are duck-typed JS objects; Rust uses the closed
//!   [`rpi_agent::session::FileEntry`] union (T01). Unknown/future entry types
//!   and entries carrying extra fields are preserved as raw JSON values
//!   ([`RawEntry`]) so load → rewrite is lossless (degradation strategy,
//!   requirements §6.6). Every record keeps its raw `serde_json::Value` as the
//!   persistence source of truth; typed views are derived for logic.
//! - Methods that perform I/O return `Result` instead of throwing
//!   (coding-standards §5). Append-write failures propagate as
//!   [`RpiError::Io`], never panic.
//! - Sync `std::fs` I/O mirrors the upstream sync methods (`appendFileSync`
//!   etc.). Async callers must wrap calls in `tokio::task::spawn_blocking`
//!   (coding-standards §6.1).
//! - `compaction` → context messages include `retainedTail` when present
//!   (session-format.md §Context Building; harness session.ts:123-127). The
//!   pinned coding-agent `sessionEntryToContextMessages` omits the tail — the
//!   doc-level behavior is ported here.
//! - `leaf` records (harness-only format feature; `LeafEntry`) are replayed in
//!   `build_index` with the harness leaf semantics — the leaf moves to the
//!   record's `targetId` (`null` clears it), `leafIdAfterEntry`
//!   (jsonl-storage.ts:134-136) — so a harness session file ending in a `leaf`
//!   record loads with the same leaf as the harness loader. The main path
//!   never writes `leaf` records, so files it produces are unaffected
//!   (upstream coding-agent's "every entry advances the leaf" behavior holds
//!   for all main-path files; the alignment only applies to harness files,
//!   matching the T16 interop contract).
//! - Randomness: `randomUUID()` → `rpi_ai::utils::uuid::random_uuid`,
//!   `uuidv7()` → `rpi_ai::utils::uuid::uuidv7` (no `rand`/`uuid` crate in the
//!   dependency baseline; non-security PRNG, see uuid.rs header).
//! - Labels are stored as a single `targetId → (label, timestamp)` map instead
//!   of upstream's two parallel maps (invariant by construction).
//! - `SessionManager.list` / `listAll` (async `/resume` selectors) are not
//!   ported here — they land with the session selector UI (T12).

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use rpi_agent::messages::AgentMessage;
use rpi_agent::session::{
    BranchSummaryEntry, CompactionEntry, CustomEntry, CustomMessageEntry, FileEntry, LabelEntry,
    MessageEntry, ModelChangeEntry, SessionEntry, SessionHeader, SessionInfoEntry,
    ThinkingLevelChangeEntry, CURRENT_SESSION_VERSION,
};
// Entry → context-message conversion and the ISO 8601 parse live in
// `rpi_agent::session` (T08): one implementation shared with the compaction
// module (stale-usage timestamp guards, agent-session.ts:1974/2030).
pub use rpi_agent::session::{parse_iso8601_ms, session_entry_to_context_messages};
use rpi_ai::types::Usage;
use rpi_ai::utils::uuid::{random_uuid, uuidv7};
use serde_json::Value;

use crate::config::{get_default_session_dir, get_default_session_dir_path};
use crate::error::RpiError;
use crate::tools::path_utils::{normalize_path, resolve_path};

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Time helpers (`Date.now()` / `new Date().toISOString()` equivalents)
// ---------------------------------------------------------------------------

/// Unix epoch milliseconds (`Date.now()`).
fn process_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Civil date from days since epoch (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// `new Date(ms).toISOString()` — `YYYY-MM-DDTHH:MM:SS.sssZ`.
fn format_iso8601_ms(ms: u64) -> String {
    let ms = ms as i64;
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, mo, d) = civil_from_days(days);
    let h = rem / 3_600_000;
    let mi = (rem % 3_600_000) / 60_000;
    let s = (rem % 60_000) / 1000;
    let milli = rem % 1000;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

pub fn now_iso8601() -> String {
    format_iso8601_ms(now_ms())
}

// ---------------------------------------------------------------------------
// Raw file records (lossless load → rewrite, requirements §6.6)
// ---------------------------------------------------------------------------

/// One parsed JSONL line: the raw JSON object is the persistence source of
/// truth (rewritten verbatim, preserving unknown fields and key order via
/// `serde_json/preserve_order`); `typed` is the parsed pinned-union view,
/// `None` for unknown/future entry types.
#[derive(Debug, Clone)]
pub struct FileEntryRecord {
    raw: Value,
    typed: Option<FileEntry>,
}

impl FileEntryRecord {
    fn from_value(raw: Value) -> Self {
        let typed = serde_json::from_value(raw.clone()).ok();
        FileEntryRecord { raw, typed }
    }

    fn from_typed(typed: FileEntry) -> Result<Self, RpiError> {
        let raw = serde_json::to_value(&typed)?;
        Ok(FileEntryRecord {
            raw,
            typed: Some(typed),
        })
    }

    /// Raw JSON object (`JSON.stringify(entry)` equivalent on rewrite).
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// Typed view, `None` for unknown/future entry types.
    pub fn typed(&self) -> Option<&FileEntry> {
        self.typed.as_ref()
    }

    /// `entry.type` (duck-typed, like upstream).
    pub fn type_tag(&self) -> &str {
        self.raw.get("type").and_then(Value::as_str).unwrap_or("")
    }

    fn is_header(&self) -> bool {
        self.type_tag() == "session"
    }

    /// `entry.id` (duck-typed; headers carry one too — callers skip headers).
    fn entry_id(&self) -> Option<&str> {
        self.raw.get("id").and_then(Value::as_str)
    }

    /// `entry.parentId` — `None` covers both `null` and missing.
    fn parent_id(&self) -> Option<&str> {
        self.raw.get("parentId").and_then(Value::as_str)
    }

    fn timestamp_str(&self) -> &str {
        self.raw
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    fn to_line(&self) -> Result<String, RpiError> {
        Ok(serde_json::to_string(&self.raw)?)
    }
}

/// Raw-preserved entry of unknown/future `type` (degradation strategy,
/// requirements §6.6): kept verbatim, navigable in the tree via
/// `id`/`parentId`, never contributes LLM context messages.
#[derive(Debug, Clone, PartialEq)]
pub struct RawEntry {
    raw: Value,
}

impl RawEntry {
    pub fn type_tag(&self) -> &str {
        self.raw.get("type").and_then(Value::as_str).unwrap_or("")
    }

    pub fn id(&self) -> &str {
        self.raw.get("id").and_then(Value::as_str).unwrap_or("")
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.raw.get("parentId").and_then(Value::as_str)
    }

    pub fn timestamp(&self) -> &str {
        self.raw
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    pub fn raw(&self) -> &Value {
        &self.raw
    }
}

/// Read-side session entry: a typed pinned entry or a raw-preserved unknown
/// entry. Returned by all "read" methods (`getEntries` etc. upstream).
#[derive(Debug, Clone, PartialEq)]
pub enum StoredEntry {
    /// Pinned entry type, plus the raw JSON object it was parsed from. `raw`
    /// is the persistence source of truth: unknown extension fields on known
    /// entry types survive branch/fork rewrites (upstream `{...entry,
    /// parentId}` spread, session-manager.ts:1426).
    Known {
        typed: Box<SessionEntry>,
        raw: Value,
    },
    Raw(RawEntry),
}

impl StoredEntry {
    fn from_record(record: &FileEntryRecord) -> Option<Self> {
        if record.is_header() {
            return None;
        }
        Some(match record.typed() {
            Some(typed) => StoredEntry::Known {
                typed: Box::new(typed.clone().into_session_entry()?),
                raw: record.raw.clone(),
            },
            None => StoredEntry::Raw(RawEntry {
                raw: record.raw.clone(),
            }),
        })
    }

    pub fn type_tag(&self) -> &str {
        match self {
            StoredEntry::Known { typed, .. } => typed.type_tag(),
            StoredEntry::Raw(e) => e.type_tag(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            StoredEntry::Known { typed, .. } => typed.id(),
            StoredEntry::Raw(e) => e.id(),
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            StoredEntry::Known { typed, .. } => typed.parent_id(),
            StoredEntry::Raw(e) => e.parent_id(),
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            StoredEntry::Known { typed, .. } => typed.timestamp(),
            StoredEntry::Raw(e) => e.timestamp(),
        }
    }

    /// Typed view for pinned entry types.
    pub fn known(&self) -> Option<&SessionEntry> {
        match self {
            StoredEntry::Known { typed, .. } => Some(typed),
            StoredEntry::Raw(_) => None,
        }
    }

    /// Raw JSON object view (persistence source of truth).
    pub fn raw_value(&self) -> &Value {
        match self {
            StoredEntry::Known { raw, .. } => raw,
            StoredEntry::Raw(e) => &e.raw,
        }
    }

    /// `{...entry, parentId}` — returns a record with the parent re-chained
    /// from the raw object, preserving unknown extension fields
    /// (createBranchedSession, session-manager.ts:1422-1428).
    fn with_parent_id(&self, parent_id: Option<String>) -> Result<FileEntryRecord, RpiError> {
        let mut raw = self.raw_value().clone();
        if let Some(obj) = raw.as_object_mut() {
            obj.insert(
                "parentId".to_owned(),
                parent_id.map(Value::String).unwrap_or(Value::Null),
            );
        }
        Ok(FileEntryRecord::from_value(raw))
    }
}

// ---------------------------------------------------------------------------
// IDs (session-manager.ts:208-228)
// ---------------------------------------------------------------------------

/// `createSessionId` — uuidv7 (session-manager.ts:208-210).
fn create_session_id() -> String {
    uuidv7()
}

/// `assertValidSessionId` (session-manager.ts:212-218). Error message is
/// byte-exact with upstream.
pub fn assert_valid_session_id(id: &str) -> Result<(), RpiError> {
    let bytes = id.as_bytes();
    let alnum = |b: u8| b.is_ascii_alphanumeric();
    let valid = !bytes.is_empty()
        && alnum(bytes[0])
        && alnum(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|&b| alnum(b) || b == b'.' || b == b'_' || b == b'-');
    if !valid {
        return Err(RpiError::Session(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and \
             '.', and start and end with an alphanumeric character"
                .to_owned(),
        ));
    }
    Ok(())
}

/// `generateId` (session-manager.ts:221-228): 8 hex chars from the head of a
/// random UUID, collision-checked; after 100 collisions falls back to a full
/// UUID.
fn generate_id(has: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let uuid = random_uuid();
        let id = &uuid[..8];
        if !has(id) {
            return id.to_owned();
        }
    }
    random_uuid()
}

// ---------------------------------------------------------------------------
// Migrations (session-manager.ts:230-296) — duck-typed on raw values
// ---------------------------------------------------------------------------

/// `migrateV1ToV2`: add id/parentId tree structure; convert compaction
/// `firstKeptEntryIndex` → `firstKeptEntryId`. Mutates in place.
fn migrate_v1_to_v2(entries: &mut [Value]) {
    // NOTE: upstream passes `ids` to generateId but never inserts into it
    // (session-manager.ts:232-243), so the collision check is vacuous here.
    // Ported as-is.
    let ids: HashSet<String> = HashSet::new();
    let mut prev_id: Option<String> = None;

    for i in 0..entries.len() {
        if entries[i].get("type").and_then(Value::as_str) == Some("session") {
            if let Some(obj) = entries[i].as_object_mut() {
                obj.insert("version".to_owned(), Value::from(2));
            }
            continue;
        }

        let id = generate_id(|candidate| ids.contains(candidate));
        // firstKeptEntryIndex → firstKeptEntryId needs the target entry's id,
        // which was assigned earlier in this same pass (targets point back).
        // Upstream deletes the key for ANY `typeof === "number"` value
        // (session-manager.ts:248-254); only non-negative integers can
        // resolve to a target (JS array indexing semantics).
        let raw_index = entries[i].get("firstKeptEntryIndex");
        let has_number_index = raw_index.is_some_and(Value::is_number);
        let first_kept_index = raw_index.and_then(Value::as_u64).map(|v| v as usize);
        let first_kept_id = first_kept_index.and_then(|idx| {
            entries
                .get(idx)
                .filter(|target| target.get("type").and_then(Value::as_str) != Some("session"))
                .and_then(|target| target.get("id").and_then(Value::as_str))
                .map(str::to_owned)
        });

        let is_compaction = entries[i].get("type").and_then(Value::as_str) == Some("compaction");
        if let Some(obj) = entries[i].as_object_mut() {
            // JS property assignment appends keys; preserve_order matches.
            obj.insert("id".to_owned(), Value::String(id.clone()));
            obj.insert(
                "parentId".to_owned(),
                prev_id.clone().map(Value::String).unwrap_or(Value::Null),
            );
            if is_compaction && has_number_index {
                if let Some(kept) = first_kept_id {
                    obj.insert("firstKeptEntryId".to_owned(), Value::String(kept));
                }
                obj.remove("firstKeptEntryIndex");
            }
        }
        prev_id = Some(id);
    }
}

/// `migrateV2ToV3`: rename `hookMessage` role → `custom`. Mutates in place.
fn migrate_v2_to_v3(entries: &mut [Value]) {
    for entry in entries.iter_mut() {
        match entry.get("type").and_then(Value::as_str) {
            Some("session") => {
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("version".to_owned(), Value::from(3));
                }
            }
            Some("message") => {
                let is_hook = entry
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("hookMessage");
                if is_hook {
                    if let Some(role) = entry
                        .as_object_mut()
                        .and_then(|obj| obj.get_mut("message"))
                        .and_then(Value::as_object_mut)
                        .and_then(|msg| msg.get_mut("role"))
                    {
                        *role = Value::String("custom".to_owned());
                    }
                }
            }
            _ => {}
        }
    }
}

/// `migrateToCurrentVersion` (session-manager.ts:281-291). Returns `true` if
/// any migration was applied.
fn migrate_to_current_version(entries: &mut [Value]) -> bool {
    // JS number semantics: `2.0` is `2`, so `2.0 < 2` is false and a
    // float-typed version must NOT be treated as v1.
    let version = entries
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("session"))
        .and_then(|h| h.get("version"))
        .and_then(Value::as_f64)
        .unwrap_or(1.0);

    if version >= f64::from(CURRENT_SESSION_VERSION) {
        return false;
    }
    if version < 2.0 {
        migrate_v1_to_v2(entries);
    }
    if version < 3.0 {
        migrate_v2_to_v3(entries);
    }
    true
}

/// `migrateSessionEntries` — exported for testing (session-manager.ts:294-296).
pub fn migrate_session_entries(entries: &mut [Value]) {
    migrate_to_current_version(entries);
}

// ---------------------------------------------------------------------------
// Line parsing + streaming load (session-manager.ts:491-556)
// ---------------------------------------------------------------------------

const SESSION_READ_BUFFER_SIZE: usize = 1024 * 1024;
const SESSION_HEADER_READ_BUFFER_SIZE: usize = 4096;
/// Bound synchronous header discovery while allowing large cwd and custom
/// metadata fields (session-manager.ts:493-494).
const MAX_SESSION_HEADER_SCAN_BYTES: usize = 1024 * 1024;

/// `SessionHeaderScanLimitError` (session-manager.ts:496-501).
#[derive(Debug)]
pub struct SessionHeaderScanLimitError(pub PathBuf);

impl std::fmt::Display for SessionHeaderScanLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Session header exceeds {MAX_SESSION_HEADER_SCAN_BYTES}-byte scan limit: {}",
            self.0.display()
        )
    }
}

impl std::error::Error for SessionHeaderScanLimitError {}

/// Errors from [`read_session_header`].
#[derive(Debug)]
pub enum ReadHeaderError {
    Io(std::io::Error),
    ScanLimit(SessionHeaderScanLimitError),
}

impl std::fmt::Display for ReadHeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadHeaderError::Io(e) => write!(f, "{e}"),
            ReadHeaderError::ScanLimit(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReadHeaderError {}

/// `parseSessionEntryLine` (session-manager.ts:503-511): skip blank and
/// malformed lines. Non-object JSON values are skipped too — they can never
/// match an entry shape (typed necessity; upstream would push them into the
/// entries array where they fail every `entry.type` check anyway).
fn parse_session_entry_line(line: &str) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    if !value.is_object() {
        return None;
    }
    Some(value)
}

/// `loadEntriesFromFile` (session-manager.ts:514-556): 1MB buffered streaming
/// read, malformed lines skipped, invalid header → empty result.
///
/// Returns the raw line values (pre-migration); [`SessionManager`] migrates.
pub fn load_entries_from_file(file_path: &Path) -> Vec<Value> {
    let resolved = PathBuf::from(normalize_path(&file_path.to_string_lossy()));
    if !resolved.exists() {
        return Vec::new();
    }
    let file = match std::fs::File::open(&resolved) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let mut entries = Vec::new();
    let mut reader = std::io::BufReader::with_capacity(SESSION_READ_BUFFER_SIZE, file);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match std::io::BufRead::read_until(&mut reader, b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                // StringDecoder('utf8') upstream replaces invalid sequences
                // with U+FFFD; from_utf8_lossy matches.
                let line = String::from_utf8_lossy(&buf);
                if let Some(value) = parse_session_entry_line(&line) {
                    entries.push(value);
                }
            }
            Err(_) => break,
        }
    }

    // Validate session header (session-manager.ts:548-554).
    if entries.is_empty() {
        return entries;
    }
    let header = &entries[0];
    let valid = header.get("type").and_then(Value::as_str) == Some("session")
        && header.get("id").and_then(Value::as_str).is_some();
    if !valid {
        return Vec::new();
    }
    entries
}

/// `parseSessionEntries` — exported for testing and as the import primitive
/// (session-manager.ts:299-314).
pub fn parse_session_entries(content: &str) -> Vec<FileEntryRecord> {
    content
        .trim()
        .split('\n')
        .filter_map(parse_session_entry_line)
        .map(FileEntryRecord::from_value)
        .collect()
}

/// Tri-state of `parseSessionHeaderCandidate` (session-manager.ts:563-569).
enum HeaderCandidate {
    /// Blank/malformed line — keep scanning.
    KeepScanning,
    /// Parsed entry that is not a session header.
    NotHeader,
    Header(SessionHeader),
}

fn parse_session_header_candidate(line: &str) -> HeaderCandidate {
    if line.trim().is_empty() {
        return HeaderCandidate::KeepScanning;
    }
    let Some(value) = parse_session_entry_line(line) else {
        return HeaderCandidate::KeepScanning;
    };
    let is_header = value.get("type").and_then(Value::as_str) == Some("session")
        && value.get("id").and_then(Value::as_str).is_some();
    if !is_header {
        return HeaderCandidate::NotHeader;
    }
    match serde_json::from_value::<SessionHeader>(value) {
        Ok(header) => HeaderCandidate::Header(header),
        // Duck-typed upstream would still return the object; typed parsing
        // requires id/timestamp/cwd shape. Files failing this remain loadable
        // via the full-load path.
        Err(_) => HeaderCandidate::NotHeader,
    }
}

/// `readSessionHeader` (session-manager.ts:571-613): 4KB buffered scan with a
/// 1MB scan limit; exceeding the limit raises
/// [`ReadHeaderError::ScanLimit`] and callers fall back to a full load.
pub fn read_session_header(file_path: &Path) -> Result<Option<SessionHeader>, ReadHeaderError> {
    let mut file = std::fs::File::open(file_path).map_err(ReadHeaderError::Io)?;
    let mut buffer = [0u8; SESSION_HEADER_READ_BUFFER_SIZE];
    let mut line_bytes: Vec<u8> = Vec::new();
    let mut scanned_bytes = 0usize;

    while scanned_bytes < MAX_SESSION_HEADER_SCAN_BYTES {
        let read_length = (MAX_SESSION_HEADER_SCAN_BYTES - scanned_bytes).min(buffer.len());
        let bytes_read = file
            .read(&mut buffer[..read_length])
            .map_err(ReadHeaderError::Io)?;
        if bytes_read == 0 {
            return Ok(
                match parse_session_header_candidate(&String::from_utf8_lossy(&line_bytes)) {
                    HeaderCandidate::Header(h) => Some(h),
                    _ => None,
                },
            );
        }
        scanned_bytes += bytes_read;

        let chunk = &buffer[..bytes_read];
        let mut line_start = 0usize;
        for (i, &b) in chunk.iter().enumerate() {
            if b == b'\n' {
                line_bytes.extend_from_slice(&chunk[line_start..i]);
                match parse_session_header_candidate(&String::from_utf8_lossy(&line_bytes)) {
                    HeaderCandidate::KeepScanning => {}
                    HeaderCandidate::NotHeader => return Ok(None),
                    HeaderCandidate::Header(h) => return Ok(Some(h)),
                }
                line_bytes.clear();
                line_start = i + 1;
            }
        }
        line_bytes.extend_from_slice(&chunk[line_start..]);
    }

    // Probe for EOF so a final header without a newline is allowed when it
    // ends exactly at the scan limit (session-manager.ts:602-609).
    let mut probe = [0u8; 1];
    if file.read(&mut probe).map_err(ReadHeaderError::Io)? == 0 {
        return Ok(
            match parse_session_header_candidate(&String::from_utf8_lossy(&line_bytes)) {
                HeaderCandidate::Header(h) => Some(h),
                _ => None,
            },
        );
    }
    Err(ReadHeaderError::ScanLimit(SessionHeaderScanLimitError(
        file_path.to_path_buf(),
    )))
}

/// `readSessionHeaderForDiscovery` (session-manager.ts:615-623): best-effort.
fn read_session_header_for_discovery(file_path: &Path) -> Option<SessionHeader> {
    read_session_header(file_path).ok().flatten()
}

fn session_header_cwd(header: &SessionHeader) -> Option<&str> {
    // `cwd` defaults to "" when absent (old sessions); "" never matches.
    if header.cwd.is_empty() {
        None
    } else {
        Some(header.cwd.as_str())
    }
}

fn session_cwd_matches(cwd: Option<&str>, resolved_cwd: &Path) -> bool {
    match cwd {
        Some(cwd) if !cwd.is_empty() => resolve_path(cwd, &process_cwd()) == resolved_cwd,
        _ => false,
    }
}

/// `findMostRecentSession` (session-manager.ts:635-656).
pub fn find_most_recent_session(session_dir: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let resolved_dir = PathBuf::from(normalize_path(&session_dir.to_string_lossy()));
    let resolved_cwd = cwd.map(|c| resolve_path(&c.to_string_lossy(), &process_cwd()));

    let read_dir = std::fs::read_dir(&resolved_dir).ok()?;
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = read_dir
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter(|p| match read_session_header_for_discovery(p) {
            Some(header) => match (&resolved_cwd, session_header_cwd(&header)) {
                (Some(rcwd), hcwd) => session_cwd_matches(hcwd, rcwd),
                (None, _) => true,
            },
            None => false,
        })
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).ok()?.modified().ok()?;
            Some((p, mtime))
        })
        .collect();
    files.sort_by_key(|(_, mtime)| *mtime);
    files.pop().map(|(p, _)| p)
}

// ---------------------------------------------------------------------------
// Context building (session-manager.ts:316-470)
// ---------------------------------------------------------------------------

/// `SessionContext["model"]` (session-manager.ts:168-172).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionModel {
    pub provider: String,
    pub model_id: String,
}

/// `SessionContext` (session-manager.ts:168-172).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<SessionModel>,
}

/// `getLatestCompactionEntry` (session-manager.ts:316-323).
pub fn get_latest_compaction_entry(entries: &[StoredEntry]) -> Option<&CompactionEntry> {
    entries.iter().rev().find_map(|e| match e.known() {
        Some(SessionEntry::Compaction(c)) => Some(c),
        _ => None,
    })
}

/// `buildSessionPath` (session-manager.ts:334-360).
///
/// `leaf_id == None` maps to upstream `leafId === null` (empty path) when
/// `default_last` is false, or to upstream's omitted-`leafId` default (last
/// entry) when true. An unknown id falls back to the last entry.
fn build_session_path<'a>(
    entries: &'a [StoredEntry],
    leaf_id: Option<&str>,
    default_last: bool,
) -> Vec<&'a StoredEntry> {
    let by_id: HashMap<&str, &StoredEntry> = entries.iter().map(|e| (e.id(), e)).collect();
    let leaf: Option<&StoredEntry> = match leaf_id {
        None if !default_last => return Vec::new(),
        None => entries.last(),
        Some(id) => by_id.get(id).copied().or(entries.last()),
    };
    let Some(mut current) = leaf else {
        return Vec::new();
    };

    let mut path = Vec::new();
    loop {
        path.push(current);
        current = match current.parent_id().and_then(|pid| by_id.get(pid)) {
            Some(parent) => parent,
            None => break,
        };
    }
    path.reverse();
    path
}

/// `getSessionContextSettings` (session-manager.ts:362-377): thinking level
/// defaults to `"off"`; model comes from the last assistant message or
/// `model_change` on the full path.
fn get_session_context_settings(path: &[&StoredEntry]) -> (String, Option<SessionModel>) {
    let mut thinking_level = "off".to_owned();
    let mut model: Option<SessionModel> = None;

    for entry in path {
        match entry.known() {
            Some(SessionEntry::ThinkingLevelChange(t)) => {
                thinking_level = t.thinking_level.clone();
            }
            Some(SessionEntry::ModelChange(m)) => {
                model = Some(SessionModel {
                    provider: m.provider.clone(),
                    model_id: m.model_id.clone(),
                });
            }
            Some(SessionEntry::Message(m)) => {
                if let AgentMessage::Assistant(a) = &m.message {
                    model = Some(SessionModel {
                        provider: a.provider.clone(),
                        model_id: a.model.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    (thinking_level, model)
}

/// `buildContextEntries` (session-manager.ts:418-454): the last compaction on
/// the path takes effect; output = compaction entry + entries from
/// `firstKeptEntryId` up to the compaction + entries after it. For the
/// `retainedTail` form (`firstKeptEntryId` absent) no earlier entries are
/// walked — the compaction is a self-contained checkpoint.
pub fn build_context_entries(entries: &[StoredEntry], leaf_id: Option<&str>) -> Vec<StoredEntry> {
    let path = build_session_path(entries, leaf_id, true);
    let compaction = path
        .iter()
        .filter_map(|e| e.known())
        .filter_map(|e| match e {
            SessionEntry::Compaction(c) => Some(c),
            _ => None,
        })
        .next_back();

    let Some(compaction) = compaction else {
        return path.into_iter().cloned().collect();
    };
    let compaction_id = compaction.id.clone();
    let first_kept_entry_id = compaction.first_kept_entry_id.clone();

    let compaction_idx = path.iter().position(|e| e.id() == compaction_id);
    let Some(compaction_idx) = compaction_idx else {
        return path.into_iter().cloned().collect();
    };

    let mut context_entries: Vec<StoredEntry> = vec![path[compaction_idx].clone()];
    let mut found_first_kept = false;
    for entry in &path[..compaction_idx] {
        if Some(entry.id()) == first_kept_entry_id.as_deref() {
            found_first_kept = true;
        }
        if found_first_kept {
            context_entries.push((*entry).clone());
        }
    }
    context_entries.extend(path[compaction_idx + 1..].iter().map(|e| (*e).clone()));
    context_entries
}

/// `buildSessionContext` (session-manager.ts:461-470).
pub fn build_session_context(entries: &[StoredEntry], leaf_id: Option<&str>) -> SessionContext {
    let path = build_session_path(entries, leaf_id, true);
    let (thinking_level, model) = get_session_context_settings(&path);
    let messages = build_context_entries(entries, leaf_id)
        .iter()
        .flat_map(|e| match e.known() {
            Some(typed) => session_entry_to_context_messages(typed),
            None => Vec::new(),
        })
        .collect();
    SessionContext {
        messages,
        thinking_level,
        model,
    }
}

// ---------------------------------------------------------------------------
// SessionManager (session-manager.ts:855-1711)
// ---------------------------------------------------------------------------

/// `NewSessionOptions` (session-manager.ts:41-44).
#[derive(Debug, Clone, Default)]
pub struct NewSessionOptions {
    pub id: Option<String>,
    pub parent_session: Option<String>,
}

/// `SessionTreeNode` (session-manager.ts:159-166).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTreeNode {
    pub entry: StoredEntry,
    pub children: Vec<SessionTreeNode>,
    pub label: Option<String>,
    pub label_timestamp: Option<String>,
}

/// `SessionForkOptions`-style fork position (harness types.ts:522-526,
/// repo-utils.ts:42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForkPosition {
    /// Fork *before* the target entry; the target must be a user message.
    #[default]
    Before,
    /// Fork *at* the target entry (included).
    At,
}

/// Options for [`SessionManager::fork_from`].
#[derive(Debug, Clone, Default)]
pub struct ForkOptions {
    pub id: Option<String>,
    /// When set, only the path to root-or-compaction from the effective leaf
    /// is copied (harness repo-utils.ts `getEntriesToFork`); when unset, all
    /// entries are copied (coding-agent `forkFrom` full copy).
    pub entry_id: Option<String>,
    pub position: Option<ForkPosition>,
}

/// Manages conversation sessions as append-only trees stored in JSONL files
/// (session-manager.ts:844-854).
#[derive(Debug)]
pub struct SessionManager {
    session_id: String,
    session_file: Option<PathBuf>,
    session_dir: PathBuf,
    cwd: PathBuf,
    persist: bool,
    flushed: bool,
    records: Vec<FileEntryRecord>,
    by_id: HashMap<String, usize>,
    /// `labelsById` + `labelTimestampsById` upstream, unified:
    /// `targetId → (label, label-entry timestamp)`.
    labels_by_id: HashMap<String, (String, String)>,
    /// Insertion order of `labels_by_id`, mirroring JS `Map` iteration order
    /// (createBranchedSession replays labels in this order,
    /// session-manager.ts:1447-1451).
    labels_order: Vec<String>,
    leaf_id: Option<String>,
}

impl SessionManager {
    fn new(
        cwd: &Path,
        session_dir: &Path,
        session_file: Option<&Path>,
        persist: bool,
        options: Option<NewSessionOptions>,
        preloaded: Option<Vec<Value>>,
    ) -> Result<Self, RpiError> {
        let cwd = resolve_path(&cwd.to_string_lossy(), &process_cwd());
        let session_dir = PathBuf::from(normalize_path(&session_dir.to_string_lossy()));
        if persist && !session_dir.as_os_str().is_empty() && !session_dir.exists() {
            std::fs::create_dir_all(&session_dir)?;
        }

        let mut manager = SessionManager {
            session_id: String::new(),
            session_file: None,
            session_dir,
            cwd,
            persist,
            flushed: false,
            records: Vec::new(),
            by_id: HashMap::new(),
            labels_by_id: HashMap::new(),
            labels_order: Vec::new(),
            leaf_id: None,
        };

        if let Some(file) = session_file {
            manager.set_session_file_internal(file, preloaded)?;
        } else {
            manager.new_session(options.unwrap_or_default())?;
        }
        Ok(manager)
    }

    /// `setSessionFile` — switch to a different session file (resume and
    /// branching, session-manager.ts:891-893).
    pub fn set_session_file(&mut self, session_file: &Path) -> Result<(), RpiError> {
        self.set_session_file_internal(session_file, None)
    }

    fn set_session_file_internal(
        &mut self,
        session_file: &Path,
        preloaded: Option<Vec<Value>>,
    ) -> Result<(), RpiError> {
        let resolved = resolve_path(&session_file.to_string_lossy(), &process_cwd());
        self.session_file = Some(resolved.clone());
        if resolved.exists() {
            let mut values = preloaded.unwrap_or_else(|| load_entries_from_file(&resolved));

            // If the file was empty, initialize it with a valid session
            // header. If it was non-empty but did not parse as a pi session,
            // fail without modifying it (session-manager.ts:900-912).
            if values.is_empty() {
                let explicit = resolved.clone();
                let size = std::fs::metadata(&explicit).map(|m| m.len()).unwrap_or(0);
                if size > 0 {
                    return Err(RpiError::Session(format!(
                        "Session file is not a valid pi session: {}",
                        explicit.display()
                    )));
                }
                self.new_session(NewSessionOptions::default())?;
                self.session_file = Some(explicit);
                self.rewrite_file()?;
                self.flushed = true;
                return Ok(());
            }

            self.session_id = values
                .iter()
                .find(|v| v.get("type").and_then(Value::as_str) == Some("session"))
                .and_then(|h| h.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(create_session_id);

            let migrated = migrate_to_current_version(&mut values);
            self.records = values
                .into_iter()
                .map(FileEntryRecord::from_value)
                .collect();
            if migrated {
                // Migrated sessions are rewritten in full (session-format.md
                // §Session Version).
                self.rewrite_file()?;
            }

            self.build_index();
            self.flushed = true;
        } else {
            let explicit = resolved.clone();
            self.new_session(NewSessionOptions::default())?;
            self.session_file = Some(explicit); // preserve explicit path from --session flag
        }
        Ok(())
    }

    /// `newSession` (session-manager.ts:930-956). Returns the new session
    /// file path (persisted sessions only).
    pub fn new_session(&mut self, options: NewSessionOptions) -> Result<Option<PathBuf>, RpiError> {
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        self.session_id = options.id.clone().unwrap_or_else(create_session_id);
        let timestamp = now_iso8601();
        let header = FileEntry::Session(SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: self.session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            parent_session: options.parent_session.clone(),
        });
        self.records = vec![FileEntryRecord::from_typed(header)?];
        self.by_id.clear();
        self.labels_by_id.clear();
        self.labels_order.clear();
        self.leaf_id = None;
        self.flushed = false;

        self.session_file = None;
        if self.persist {
            let file_timestamp = timestamp.replace([':', '.'], "-");
            self.session_file = Some(
                self.get_session_dir()
                    .join(format!("{file_timestamp}_{}.jsonl", self.session_id)),
            );
        }
        Ok(self.session_file.clone())
    }

    /// JS `Map.set` semantics: a new key appends to the insertion order, an
    /// existing key keeps its position.
    fn set_label(&mut self, target: &str, label: String, timestamp: String) {
        if !self.labels_by_id.contains_key(target) {
            self.labels_order.push(target.to_owned());
        }
        self.labels_by_id
            .insert(target.to_owned(), (label, timestamp));
    }

    /// JS `Map.delete` semantics: re-setting a cleared label re-appends it at
    /// the end of the insertion order.
    fn clear_label(&mut self, target: &str) {
        if self.labels_by_id.remove(target).is_some() {
            self.labels_order.retain(|t| t != target);
        }
    }

    /// `_buildIndex` (session-manager.ts:958-977).
    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels_by_id.clear();
        self.labels_order.clear();
        self.leaf_id = None;
        // Label ops are collected during the scan and applied afterwards so
        // the records borrow can end first (order of ops is replay order).
        let mut label_ops: Vec<(String, Option<String>, String)> = Vec::new();
        for (i, record) in self.records.iter().enumerate() {
            if record.is_header() {
                continue;
            }
            let Some(id) = record.entry_id() else {
                continue;
            };
            self.by_id.insert(id.to_owned(), i);
            // Harness `leaf` records move the leaf to their `targetId` (`null`
            // clears it) instead of advancing to their own id — `leafIdAfterEntry`
            // (jsonl-storage.ts:134-136). The main path never writes leaf records,
            // so this only affects harness-produced files (see header note).
            self.leaf_id = if record.type_tag() == "leaf" {
                record
                    .raw
                    .get("targetId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            } else {
                Some(id.to_owned())
            };
            // Duck-typed label replay (entry.type === "label").
            if record.type_tag() == "label" {
                let target = record.raw.get("targetId").and_then(Value::as_str);
                let label = record.raw.get("label").and_then(Value::as_str);
                if let Some(target) = target {
                    // `if (entry.label)` upstream: empty string clears too.
                    match label {
                        Some(label) if !label.is_empty() => label_ops.push((
                            target.to_owned(),
                            Some(label.to_owned()),
                            record.timestamp_str().to_owned(),
                        )),
                        _ => label_ops.push((target.to_owned(), None, String::new())),
                    }
                }
            }
        }
        for (target, label, timestamp) in label_ops {
            match label {
                Some(label) => self.set_label(&target, label, timestamp),
                None => self.clear_label(&target),
            }
        }
    }

    /// `_rewriteFile` (session-manager.ts:979-989): truncate + write all.
    fn rewrite_file(&self) -> Result<(), RpiError> {
        if !self.persist {
            return Ok(());
        }
        let Some(file) = &self.session_file else {
            return Ok(());
        };
        let mut fd = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(file)?;
        for record in &self.records {
            fd.write_all(record.to_line()?.as_bytes())?;
            fd.write_all(b"\n")?;
        }
        Ok(())
    }

    /// `_persist` (session-manager.ts:1015-1042): deferred persistence — the
    /// file is not created until the first assistant message (the `flushed`
    /// flag plus `wx` exclusive create); afterwards entries are appended
    /// directly. No file locking (G4 red line).
    fn persist_appended_entry(&mut self) -> Result<(), RpiError> {
        if !self.persist {
            return Ok(());
        }
        let Some(file) = self.session_file.clone() else {
            return Ok(());
        };
        // Invariant: append_entry pushes before persisting, so a last record
        // always exists here.
        let entry_line = match self.records.last() {
            Some(record) => record.to_line()?,
            None => return Ok(()),
        };

        // Duck-typed like upstream: e.type === "message" && role assistant.
        let has_assistant = self.records.iter().any(|r| {
            r.type_tag() == "message"
                && r.raw
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("assistant")
        });
        if !has_assistant {
            if self.flushed {
                let mut fd = std::fs::OpenOptions::new().append(true).open(&file)?;
                fd.write_all(entry_line.as_bytes())?;
                fd.write_all(b"\n")?;
            }
            // Not flushed: leave it false so when the assistant arrives all
            // entries get written.
            return Ok(());
        }

        if !self.flushed {
            // `wx` — exclusive create; EEXIST propagates as an error.
            let mut fd = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)?;
            for record in &self.records {
                fd.write_all(record.to_line()?.as_bytes())?;
                fd.write_all(b"\n")?;
            }
            self.flushed = true;
        } else {
            let mut fd = std::fs::OpenOptions::new().append(true).open(&file)?;
            fd.write_all(entry_line.as_bytes())?;
            fd.write_all(b"\n")?;
        }
        Ok(())
    }

    /// `_appendEntry` (session-manager.ts:1044-1049): push, index, advance
    /// leaf, then persist the appended entry.
    fn append_entry(&mut self, typed: FileEntry) -> Result<String, RpiError> {
        let record = FileEntryRecord::from_typed(typed)?;
        let id = record.entry_id().unwrap_or_default().to_owned();
        self.by_id.insert(id.clone(), self.records.len());
        self.leaf_id = Some(id.clone());
        self.records.push(record);
        self.persist_appended_entry()?;
        Ok(id)
    }

    fn next_entry_id(&self) -> String {
        generate_id(|candidate| self.by_id.contains_key(candidate))
    }

    // -----------------------------------------------------------------------
    // Appending (all return the entry id; I/O failures propagate)
    // -----------------------------------------------------------------------

    /// `appendMessage` (session-manager.ts:1057-1067).
    ///
    /// `CompactionSummaryMessage` / `BranchSummaryMessage` are rejected — they
    /// must be appended via `appendCompaction` / `appendBranchSummary` (the
    /// upstream doc comment; TS enforces this at the type level).
    pub fn append_message(&mut self, message: AgentMessage) -> Result<String, RpiError> {
        match &message {
            AgentMessage::CompactionSummary(_) | AgentMessage::BranchSummary(_) => {
                return Err(RpiError::Session(
                    "compactionSummary/branchSummary messages must be appended via \
                     appendCompaction/branchWithSummary"
                        .to_owned(),
                ));
            }
            _ => {}
        }
        let entry = FileEntry::Message(MessageEntry {
            id: self.next_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: now_iso8601(),
            message,
        });
        self.append_entry(entry)
    }

    /// `appendThinkingLevelChange` (session-manager.ts:1070-1080).
    pub fn append_thinking_level_change(
        &mut self,
        thinking_level: &str,
    ) -> Result<String, RpiError> {
        let entry = FileEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            id: self.next_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: now_iso8601(),
            thinking_level: thinking_level.to_owned(),
        });
        self.append_entry(entry)
    }

    /// `appendModelChange` (session-manager.ts:1083-1094).
    pub fn append_model_change(
        &mut self,
        provider: &str,
        model_id: &str,
    ) -> Result<String, RpiError> {
        let entry = FileEntry::ModelChange(ModelChangeEntry {
            id: self.next_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: now_iso8601(),
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        });
        self.append_entry(entry)
    }

    /// `appendCompaction` (session-manager.ts:1097-1119). The main path
    /// writes only the `firstKeptEntryId` form (ADR-0003 §1).
    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: &str,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> Result<String, RpiError> {
        let entry = FileEntry::Compaction(CompactionEntry {
            id: self.next_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: now_iso8601(),
            summary: summary.to_owned(),
            first_kept_entry_id: Some(first_kept_entry_id.to_owned()),
            tokens_before,
            retained_tail: None,
            details,
            usage,
            from_hook,
        });
        self.append_entry(entry)
    }

    /// `appendCustomEntry` (session-manager.ts:1122-1133).
    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<String, RpiError> {
        let entry = FileEntry::Custom(CustomEntry {
            custom_type: custom_type.to_owned(),
            data,
            id: self.next_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: now_iso8601(),
        });
        self.append_entry(entry)
    }

    /// `appendSessionInfo` (session-manager.ts:1136-1147): `\r\n` runs are
    /// replaced with a space and the result trimmed.
    pub fn append_session_info(&mut self, name: &str) -> Result<String, RpiError> {
        let mut sanitized = String::with_capacity(name.len());
        let mut in_run = false;
        for c in name.trim().chars() {
            if c == '\r' || c == '\n' {
                if !in_run {
                    sanitized.push(' ');
                    in_run = true;
                }
            } else {
                sanitized.push(c);
                in_run = false;
            }
        }
        let sanitized = sanitized.trim().to_owned();
        let entry = FileEntry::SessionInfo(SessionInfoEntry {
            id: self.next_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: now_iso8601(),
            name: Some(sanitized),
        });
        self.append_entry(entry)
    }

    /// `getSessionName` (session-manager.ts:1150-1161): latest session_info;
    /// empty names explicitly clear the title.
    pub fn get_session_name(&self) -> Option<String> {
        for record in self.records.iter().rev() {
            if record.type_tag() == "session_info" {
                let name = record.raw.get("name").and_then(Value::as_str);
                return name.and_then(|n| {
                    let trimmed = n.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_owned())
                    }
                });
            }
        }
        None
    }

    /// `appendCustomMessageEntry` (session-manager.ts:1171-1189).
    pub fn append_custom_message_entry(
        &mut self,
        custom_type: &str,
        content: rpi_ai::types::UserContent,
        display: bool,
        details: Option<Value>,
    ) -> Result<String, RpiError> {
        let entry = FileEntry::CustomMessage(CustomMessageEntry {
            custom_type: custom_type.to_owned(),
            content,
            display,
            details,
            id: self.next_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: now_iso8601(),
        });
        self.append_entry(entry)
    }

    // -----------------------------------------------------------------------
    // Tree traversal (session-manager.ts:1191-1348)
    // -----------------------------------------------------------------------

    /// `getLeafId`.
    pub fn get_leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// `getLeafEntry`.
    pub fn get_leaf_entry(&self) -> Option<StoredEntry> {
        self.leaf_id
            .as_deref()
            .and_then(|id| self.by_id.get(id))
            .and_then(|&i| StoredEntry::from_record(&self.records[i]))
    }

    /// `getEntry`.
    pub fn get_entry(&self, id: &str) -> Option<StoredEntry> {
        self.by_id
            .get(id)
            .and_then(|&i| StoredEntry::from_record(&self.records[i]))
    }

    /// `getChildren` — direct children in append (file) order.
    pub fn get_children(&self, parent_id: &str) -> Vec<StoredEntry> {
        self.records
            .iter()
            .filter(|r| !r.is_header() && r.parent_id() == Some(parent_id))
            .filter_map(StoredEntry::from_record)
            .collect()
    }

    /// `getLabel`.
    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels_by_id.get(id).map(|(label, _)| label.as_str())
    }

    /// `appendLabelChange` (session-manager.ts:1232-1253): `None`/empty label
    /// clears.
    pub fn append_label_change(
        &mut self,
        target_id: &str,
        label: Option<&str>,
    ) -> Result<String, RpiError> {
        if !self.by_id.contains_key(target_id) {
            return Err(RpiError::Session(format!("Entry {target_id} not found")));
        }
        let timestamp = now_iso8601();
        let entry = FileEntry::Label(LabelEntry {
            id: self.next_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: timestamp.clone(),
            target_id: target_id.to_owned(),
            label: label.map(str::to_owned),
        });
        let id = self.append_entry(entry)?;
        match label {
            Some(label) if !label.is_empty() => {
                self.set_label(target_id, label.to_owned(), timestamp);
            }
            _ => {
                self.clear_label(target_id);
            }
        }
        Ok(id)
    }

    /// `getBranch` (session-manager.ts:1260-1270): walk from entry (default
    /// current leaf) to root, in path order.
    pub fn get_branch(&self, from_id: Option<&str>) -> Vec<StoredEntry> {
        let entries = self.get_entries();
        match from_id {
            // Method-level `getBranch`: an unknown id yields `[]`; the
            // last-entry fallback only exists in the free `buildSessionPath`
            // (session-manager.ts:1260-1270 vs :347).
            Some(id) => {
                if !self.by_id.contains_key(id) {
                    return Vec::new();
                }
                build_session_path(&entries, Some(id), true)
                    .into_iter()
                    .cloned()
                    .collect()
            }
            None => {
                // leafId null → empty; set → walk (upstream passes this.leafId).
                match &self.leaf_id {
                    None => Vec::new(),
                    Some(id) => build_session_path(&entries, Some(id), true)
                        .into_iter()
                        .cloned()
                        .collect(),
                }
            }
        }
    }

    /// `buildContextEntries` — active compaction-aware entry list from the
    /// current leaf.
    pub fn build_context_entries(&self) -> Vec<StoredEntry> {
        let entries = self.get_entries();
        match &self.leaf_id {
            None => Vec::new(),
            Some(id) => build_context_entries(&entries, Some(id)),
        }
    }

    /// `buildSessionContext` — what gets sent to the LLM.
    pub fn build_session_context(&self) -> SessionContext {
        let entries = self.get_entries();
        let path = match &self.leaf_id {
            None => Vec::new(),
            Some(id) => build_session_path(&entries, Some(id), true),
        };
        let (thinking_level, model) = get_session_context_settings(&path);
        let messages = self
            .build_context_entries()
            .iter()
            .flat_map(|e| match e.known() {
                Some(typed) => session_entry_to_context_messages(typed),
                None => Vec::new(),
            })
            .collect();
        SessionContext {
            messages,
            thinking_level,
            model,
        }
    }

    /// `getHeader`.
    pub fn get_header(&self) -> Option<&SessionHeader> {
        self.records
            .iter()
            .find(|r| r.is_header())
            .and_then(|r| r.typed())
            .and_then(|t| match t {
                FileEntry::Session(h) => Some(h),
                _ => None,
            })
    }

    /// Raw JSON header object — upstream `getHeader()` returns the parsed
    /// object as-is (a type assertion, not a re-serialization), so unknown
    /// extension fields survive into the HTML export; the typed view above
    /// drops them (T14 review fix).
    pub fn get_header_raw(&self) -> Option<&Value> {
        self.records.iter().find(|r| r.is_header()).map(|r| r.raw())
    }

    /// `getEntries` — all entries, header excluded.
    pub fn get_entries(&self) -> Vec<StoredEntry> {
        self.records
            .iter()
            .filter_map(StoredEntry::from_record)
            .collect()
    }

    /// `getTree` (session-manager.ts:1310-1348): orphaned entries (broken
    /// parent chain) are returned as roots; children sorted by timestamp,
    /// oldest first.
    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        let entries = self.get_entries();
        let mut nodes: HashMap<String, SessionTreeNode> = entries
            .iter()
            .map(|e| {
                let label = self.labels_by_id.get(e.id());
                (
                    e.id().to_owned(),
                    SessionTreeNode {
                        entry: e.clone(),
                        children: Vec::new(),
                        label: label.map(|(l, _)| l.clone()),
                        label_timestamp: label.map(|(_, ts)| ts.clone()),
                    },
                )
            })
            .collect();

        // Attach children to parents (or roots). Collect relations first to
        // satisfy the borrow checker; a node id may appear once in the tree.
        let mut roots: Vec<String> = Vec::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for entry in &entries {
            match entry.parent_id() {
                None => roots.push(entry.id().to_owned()),
                Some(parent) if parent == entry.id() => roots.push(entry.id().to_owned()),
                Some(parent) if nodes.contains_key(parent) => {
                    children
                        .entry(parent.to_owned())
                        .or_default()
                        .push(entry.id().to_owned());
                }
                // Orphan — treat as root.
                Some(_) => roots.push(entry.id().to_owned()),
            }
        }

        // Build the tree bottom-up with an explicit stack (upstream uses an
        // iterative approach to avoid stack overflow on deep trees,
        // session-manager.ts:1338-1345).
        fn assemble(
            root_id: &str,
            nodes: &mut HashMap<String, SessionTreeNode>,
            children: &HashMap<String, Vec<String>>,
        ) -> Option<SessionTreeNode> {
            enum Frame {
                Enter(String),
                Exit(String),
            }
            let mut stack = vec![Frame::Enter(root_id.to_string())];
            let mut done: HashMap<String, SessionTreeNode> = HashMap::new();
            while let Some(frame) = stack.pop() {
                match frame {
                    Frame::Enter(id) => {
                        if done.contains_key(&id) || !nodes.contains_key(&id) {
                            continue;
                        }
                        stack.push(Frame::Exit(id.clone()));
                        if let Some(kids) = children.get(&id) {
                            for kid in kids {
                                stack.push(Frame::Enter(kid.clone()));
                            }
                        }
                    }
                    Frame::Exit(id) => {
                        let Some(mut node) = nodes.remove(&id) else {
                            continue;
                        };
                        let mut child_nodes: Vec<SessionTreeNode> = Vec::new();
                        if let Some(kids) = children.get(&id) {
                            for kid in kids {
                                if let Some(child) = done.remove(kid) {
                                    child_nodes.push(child);
                                }
                            }
                        }
                        // Sort children by timestamp, oldest first.
                        // Unparseable timestamps compare equal (upstream NaN
                        // comparator behaves as no-op in V8's stable sort;
                        // Rust sort_by is stable too).
                        child_nodes.sort_by(|a, b| {
                            match (
                                parse_iso8601_ms(a.entry.timestamp()),
                                parse_iso8601_ms(b.entry.timestamp()),
                            ) {
                                (Some(x), Some(y)) => x.cmp(&y),
                                _ => std::cmp::Ordering::Equal,
                            }
                        });
                        node.children = child_nodes;
                        done.insert(id, node);
                    }
                }
            }
            done.remove(root_id)
        }

        let mut result = Vec::new();
        for root_id in &roots {
            if let Some(node) = assemble(root_id, &mut nodes, &children) {
                result.push(node);
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // Branching (session-manager.ts:1350-1512)
    // -----------------------------------------------------------------------

    /// `branch` (session-manager.ts:1360-1365): move the leaf pointer.
    pub fn branch(&mut self, branch_from_id: &str) -> Result<(), RpiError> {
        if !self.by_id.contains_key(branch_from_id) {
            return Err(RpiError::Session(format!(
                "Entry {branch_from_id} not found"
            )));
        }
        self.leaf_id = Some(branch_from_id.to_owned());
        Ok(())
    }

    /// `resetLeaf` (session-manager.ts:1372-1374).
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    /// `branchWithSummary` (session-manager.ts:1381-1405).
    pub fn branch_with_summary(
        &mut self,
        branch_from_id: Option<&str>,
        summary: &str,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> Result<String, RpiError> {
        if let Some(id) = branch_from_id {
            if !self.by_id.contains_key(id) {
                return Err(RpiError::Session(format!("Entry {id} not found")));
            }
        }
        self.leaf_id = branch_from_id.map(str::to_owned);
        let entry = FileEntry::BranchSummary(BranchSummaryEntry {
            id: self.next_entry_id(),
            parent_id: branch_from_id.map(str::to_owned),
            timestamp: now_iso8601(),
            from_id: branch_from_id.unwrap_or("root").to_owned(),
            summary: summary.to_owned(),
            details,
            usage,
            from_hook,
        });
        self.append_entry(entry)
    }

    /// `createBranchedSession` (session-manager.ts:1412-1512): extract the
    /// path root→leaf into a new session; label entries are filtered from the
    /// path, the path is re-chained by new parentIds, and resolved labels are
    /// re-appended at the tail. Returns the new session file (persisted only).
    pub fn create_branched_session(&mut self, leaf_id: &str) -> Result<Option<PathBuf>, RpiError> {
        // session-manager.ts:1414-1416 — explicit lookup; unknown ids throw
        // before any branch/persist handling.
        if !self.by_id.contains_key(leaf_id) {
            return Err(RpiError::Session(format!("Entry {leaf_id} not found")));
        }
        let previous_session_file = self.session_file.clone();
        let path = self.get_branch(Some(leaf_id));
        if path.is_empty() {
            return Err(RpiError::Session(format!("Entry {leaf_id} not found")));
        }

        // Filter out label entries; re-chain the retained path so children of
        // removed labels are not orphaned (session-manager.ts:1422-1428).
        let mut path_without_labels: Vec<StoredEntry> = Vec::new();
        let mut new_records: Vec<FileEntryRecord> = Vec::new();
        let mut path_parent_id: Option<String> = None;
        for entry in &path {
            if entry.type_tag() == "label" {
                continue;
            }
            let record = entry.with_parent_id(path_parent_id.clone())?;
            path_parent_id = Some(record.entry_id().unwrap_or_default().to_owned());
            let view = StoredEntry::from_record(&record);
            new_records.push(record);
            if let Some(view) = view {
                path_without_labels.push(view);
            }
        }

        let new_session_id = create_session_id();
        let timestamp = now_iso8601();
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let new_session_file = self
            .get_session_dir()
            .join(format!("{file_timestamp}_{new_session_id}.jsonl"));

        let header = FileEntry::Session(SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: new_session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            parent_session: if self.persist {
                previous_session_file
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
            } else {
                None
            },
        });

        // Collect labels for entries in the path, in JS `Map` insertion order
        // (session-manager.ts:1445-1451).
        let path_entry_ids: HashSet<String> = path_without_labels
            .iter()
            .map(|e| e.id().to_owned())
            .collect();
        let labels_to_write: Vec<(String, String, String)> = self
            .labels_order
            .iter()
            .filter(|target| path_entry_ids.contains(*target))
            .filter_map(|target| {
                self.labels_by_id
                    .get(target)
                    .map(|(label, ts)| (target.clone(), label.clone(), ts.clone()))
            })
            .collect();

        // Rebuild label entries chained after the last path entry
        // (session-manager.ts:1455-1470 / 1494-1507).
        let mut id_pool: HashSet<String> = path_entry_ids.clone();
        let mut parent_id = path_without_labels.last().map(|e| e.id().to_owned());
        let mut label_records: Vec<FileEntryRecord> = Vec::new();
        for (target_id, label, label_timestamp) in labels_to_write {
            let label_entry_id = generate_id(|candidate| id_pool.contains(candidate));
            id_pool.insert(label_entry_id.clone());
            let record = FileEntryRecord::from_typed(FileEntry::Label(LabelEntry {
                id: label_entry_id,
                parent_id: parent_id.clone(),
                timestamp: label_timestamp,
                target_id,
                label: Some(label),
            }))?;
            parent_id = Some(record.entry_id().unwrap_or_default().to_owned());
            label_records.push(record);
        }

        let mut records = vec![FileEntryRecord::from_typed(header)?];
        records.extend(new_records);
        records.extend(label_records);
        self.records = records;
        self.session_id = new_session_id;
        self.build_index();

        if self.persist {
            self.session_file = Some(new_session_file.clone());
            // Only write the file now if it contains an assistant message;
            // otherwise defer to persist_entry on the first assistant
            // response, matching the newSession() contract
            // (session-manager.ts:1477-1488).
            let has_assistant = self.records.iter().any(|r| {
                r.type_tag() == "message"
                    && r.raw
                        .get("message")
                        .and_then(|m| m.get("role"))
                        .and_then(Value::as_str)
                        == Some("assistant")
            });
            if has_assistant {
                self.rewrite_file()?;
                self.flushed = true;
            } else {
                self.flushed = false;
            }
            return Ok(Some(new_session_file));
        }
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Info (session-manager.ts:991-1013)
    // -----------------------------------------------------------------------

    /// `isPersisted`.
    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    /// `getCwd`.
    pub fn get_cwd(&self) -> &Path {
        &self.cwd
    }

    /// `getSessionDir`.
    pub fn get_session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// `usesDefaultSessionDir` (session-manager.ts:1003-1005).
    pub fn uses_default_session_dir(&self) -> bool {
        self.session_dir == get_default_session_dir_path(&self.cwd, None)
    }

    /// `getSessionId`.
    pub fn get_session_id(&self) -> &str {
        &self.session_id
    }

    /// `getSessionFile` — `None` for in-memory sessions.
    pub fn get_session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    // -----------------------------------------------------------------------
    // Static creation methods (session-manager.ts:1514-1630)
    // -----------------------------------------------------------------------

    /// `SessionManager.create` (session-manager.ts:1519-1522).
    pub fn create(
        cwd: &Path,
        session_dir: Option<&Path>,
        options: NewSessionOptions,
    ) -> Result<Self, RpiError> {
        let dir = match session_dir {
            Some(dir) => PathBuf::from(normalize_path(&dir.to_string_lossy())),
            None => get_default_session_dir(cwd)?,
        };
        SessionManager::new(cwd, &dir, None, true, Some(options), None)
    }

    /// `SessionManager.open` (session-manager.ts:1530-1550).
    ///
    /// The bounded header scan is only a discovery optimization: when it hits
    /// the 1MB scan limit, a full load remains authoritative.
    pub fn open(
        path: &Path,
        session_dir: Option<&Path>,
        cwd_override: Option<&Path>,
    ) -> Result<Self, RpiError> {
        let resolved = resolve_path(&path.to_string_lossy(), &process_cwd());
        let mut header: Option<SessionHeader> = None;
        let mut preloaded: Option<Vec<Value>> = None;
        if cwd_override.is_none() && resolved.exists() {
            match read_session_header(&resolved) {
                Ok(h) => header = h,
                Err(ReadHeaderError::ScanLimit(_)) => {
                    preloaded = Some(load_entries_from_file(&resolved));
                    header = preloaded
                        .as_ref()
                        .and_then(|values| values.first())
                        .filter(|v| v.get("type").and_then(Value::as_str) == Some("session"))
                        .and_then(|v| serde_json::from_value::<SessionHeader>(v.clone()).ok());
                }
                Err(ReadHeaderError::Io(e)) => return Err(RpiError::Io(e)),
            }
        }
        let cwd = match cwd_override {
            Some(c) => c.to_path_buf(),
            None => header
                .as_ref()
                .and_then(|h| session_header_cwd(h).map(PathBuf::from))
                .unwrap_or_else(process_cwd),
        };
        // If no sessionDir provided, derive from the file's parent directory.
        let dir = match session_dir {
            Some(d) => PathBuf::from(normalize_path(&d.to_string_lossy())),
            None => resolved
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
        };
        SessionManager::new(&cwd, &dir, Some(&resolved), true, None, preloaded)
    }

    /// `SessionManager.continueRecent` (session-manager.ts:1557-1565).
    pub fn continue_recent(cwd: &Path, session_dir: Option<&Path>) -> Result<Self, RpiError> {
        let dir = match session_dir {
            Some(d) => PathBuf::from(normalize_path(&d.to_string_lossy())),
            None => get_default_session_dir(cwd)?,
        };
        let filter_cwd = session_dir.is_some() && dir != get_default_session_dir_path(cwd, None);
        let most_recent = find_most_recent_session(&dir, if filter_cwd { Some(cwd) } else { None });
        match most_recent {
            Some(file) => SessionManager::new(cwd, &dir, Some(&file), true, None, None),
            None => SessionManager::new(cwd, &dir, None, true, None, None),
        }
    }

    /// `SessionManager.inMemory` (session-manager.ts:1568-1570) — the
    /// `--no-session` memory session.
    pub fn in_memory(cwd: Option<&Path>, options: NewSessionOptions) -> Result<Self, RpiError> {
        let cwd = cwd.map(Path::to_path_buf).unwrap_or_else(process_cwd);
        SessionManager::new(&cwd, Path::new(""), None, false, Some(options), None)
    }

    /// `getEntriesToFork` (harness repo-utils.ts:32-51): `position: "at"`
    /// includes the target; `"before"` (default) requires a user-message
    /// target and starts from its parent.
    fn entries_to_fork(&self, options: &ForkOptions) -> Result<Vec<StoredEntry>, RpiError> {
        let entries = self.get_entries();
        let Some(entry_id) = &options.entry_id else {
            return Ok(entries);
        };
        let Some(target) = self.get_entry(entry_id) else {
            return Err(RpiError::Session(format!("Entry {entry_id} not found")));
        };
        let effective_leaf_id: Option<String> = match options.position.unwrap_or_default() {
            ForkPosition::At => Some(target.id().to_owned()),
            ForkPosition::Before => {
                let is_user_message = matches!(
                    target.known(),
                    Some(SessionEntry::Message(m)) if matches!(m.message, AgentMessage::User(_))
                );
                if !is_user_message {
                    return Err(RpiError::Session(format!(
                        "Entry {entry_id} is not a user message"
                    )));
                }
                target.parent_id().map(str::to_owned)
            }
        };
        path_to_root_or_compaction(&entries, effective_leaf_id.as_deref())
    }

    /// `getPathToRootOrCompaction` is also used standalone (SessionStorage
    /// trait alignment); see free fn below.
    pub fn get_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<StoredEntry>, RpiError> {
        let entries = self.get_entries();
        path_to_root_or_compaction(&entries, leaf_id)
    }

    /// `SessionManager.forkFrom` (session-manager.ts:1579-1630): new header
    /// pointing at the source via `parentSession`, full copy of the source
    /// entries (or the `getEntriesToFork` selection when `entry_id` is set),
    /// `wx` exclusive create.
    pub fn fork_from(
        source_path: &Path,
        target_cwd: &Path,
        session_dir: Option<&Path>,
        options: ForkOptions,
    ) -> Result<Self, RpiError> {
        let resolved_source = resolve_path(&source_path.to_string_lossy(), &process_cwd());
        let resolved_target_cwd = resolve_path(&target_cwd.to_string_lossy(), &process_cwd());
        let source_values = load_entries_from_file(&resolved_source);
        if source_values.is_empty() {
            return Err(RpiError::Session(format!(
                "Cannot fork: source session file is empty or invalid: {}",
                resolved_source.display()
            )));
        }
        let has_header = source_values
            .iter()
            .any(|v| v.get("type").and_then(Value::as_str) == Some("session"));
        if !has_header {
            return Err(RpiError::Session(format!(
                "Cannot fork: source session has no header: {}",
                resolved_source.display()
            )));
        }

        let dir = match session_dir {
            Some(d) => PathBuf::from(normalize_path(&d.to_string_lossy())),
            None => get_default_session_dir(&resolved_target_cwd)?,
        };
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
        }

        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        let new_session_id = options.id.clone().unwrap_or_else(create_session_id);
        let timestamp = now_iso8601();
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let new_session_file = dir.join(format!("{file_timestamp}_{new_session_id}.jsonl"));

        // Entries to copy: full copy (coding-agent), or the harness
        // getEntriesToFork selection when entry_id is given. Selection runs
        // against a temporary manager over the source records, BEFORE the
        // `wx` create so a validation failure leaves no orphan header file
        // on disk (harness jsonl-repo.ts:138-147).
        let copy_lines: Vec<String> = if options.entry_id.is_some() {
            let source_records: Vec<FileEntryRecord> = source_values
                .iter()
                .map(|v| FileEntryRecord::from_value(v.clone()))
                .collect();
            let source_manager = SessionManager {
                session_id: String::new(),
                session_file: None,
                session_dir: dir.clone(),
                cwd: resolved_target_cwd.clone(),
                persist: false,
                flushed: false,
                by_id: {
                    let mut map = HashMap::new();
                    for (i, r) in source_records.iter().enumerate() {
                        if !r.is_header() {
                            if let Some(id) = r.entry_id() {
                                map.insert(id.to_owned(), i);
                            }
                        }
                    }
                    map
                },
                records: source_records,
                labels_by_id: HashMap::new(),
                labels_order: Vec::new(),
                leaf_id: None,
            };
            let selected = source_manager.entries_to_fork(&options)?;
            let mut lines = Vec::new();
            for entry in &selected {
                lines.push(serde_json::to_string(entry.raw_value())?);
            }
            lines
        } else {
            let mut lines = Vec::new();
            for value in &source_values {
                if value.get("type").and_then(Value::as_str) == Some("session") {
                    continue;
                }
                lines.push(serde_json::to_string(value)?);
            }
            lines
        };

        let new_header = FileEntry::Session(SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: new_session_id,
            timestamp,
            cwd: resolved_target_cwd.to_string_lossy().into_owned(),
            parent_session: Some(resolved_source.to_string_lossy().into_owned()),
        });
        let header_record = FileEntryRecord::from_typed(new_header)?;
        // `wx` exclusive create (writeFileSync flag "wx", session-manager.ts:1620).
        let mut fd = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&new_session_file)?;
        fd.write_all(header_record.to_line()?.as_bytes())?;
        fd.write_all(b"\n")?;
        for line in &copy_lines {
            fd.write_all(line.as_bytes())?;
            fd.write_all(b"\n")?;
        }
        drop(fd);

        SessionManager::new(
            &resolved_target_cwd,
            &dir,
            Some(&new_session_file),
            true,
            None,
            None,
        )
    }

    // -----------------------------------------------------------------------
    // Import / export JSONL primitives (CLI wiring lands in T10/T14)
    // -----------------------------------------------------------------------

    /// Export the session as JSONL text (header + entries, one JSON object
    /// per line, trailing newline). The inverse of [`parse_session_entries`].
    pub fn export_jsonl(&self) -> Result<String, RpiError> {
        let mut out = String::new();
        for record in &self.records {
            out.push_str(&record.to_line()?);
            out.push('\n');
        }
        Ok(out)
    }
}

/// `getPathToRootOrCompaction` (harness jsonl-storage.ts:344-370): walk from
/// leaf to root, stopping at the first compaction with a `retainedTail`
/// (self-contained checkpoint) or, for the `firstKeptEntryId` form, after
/// including that entry.
pub fn path_to_root_or_compaction(
    entries: &[StoredEntry],
    leaf_id: Option<&str>,
) -> Result<Vec<StoredEntry>, RpiError> {
    let Some(leaf_id) = leaf_id else {
        return Ok(Vec::new());
    };
    let by_id: HashMap<&str, &StoredEntry> = entries.iter().map(|e| (e.id(), e)).collect();
    let mut path: Vec<&StoredEntry> = Vec::new();
    let mut stop_at_entry_id: Option<String> = None;
    let mut current: Option<&StoredEntry> = by_id.get(leaf_id).copied();
    if current.is_none() {
        return Err(RpiError::Session(format!("Entry {leaf_id} not found")));
    }
    while let Some(entry) = current {
        path.push(entry);
        if stop_at_entry_id.as_deref() == Some(entry.id()) {
            break;
        }
        if let Some(SessionEntry::Compaction(c)) = entry.known() {
            if c.retained_tail.is_some() {
                break;
            }
            stop_at_entry_id = c.first_kept_entry_id.clone();
        }
        let Some(parent_id) = entry.parent_id() else {
            break;
        };
        current = match by_id.get(parent_id) {
            Some(parent) => Some(parent),
            None => {
                return Err(RpiError::Session(format!("Entry {parent_id} not found")));
            }
        };
    }
    path.reverse();
    Ok(path.into_iter().cloned().collect())
}

// ---------------------------------------------------------------------------
// Session discovery: list / listAll (session-manager.ts:1638-1711)
// ---------------------------------------------------------------------------
//
// Assigned to T12 in the T07 landing note (D-012) but pulled forward: T10's
// `--session` / `--fork` / `--session-id` resolution needs it. Sequential
// reads replace upstream's 10-way concurrency limit and progress callbacks
// (ordering is identical after the `modified` sort; only latency differs).

/// `SessionInfo` (session-manager.ts:174-186). Timestamps are epoch ms
/// (upstream `Date`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    /// Working directory where the session was started. Empty for old
    /// sessions.
    pub cwd: String,
    pub name: Option<String>,
    pub parent_session_path: Option<String>,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub message_count: u64,
    pub first_message: String,
    pub all_messages_text: String,
}

/// `isMessageWithContent` + `extractTextContent`
/// (session-manager.ts:658-671).
fn extract_message_text(message: &AgentMessage) -> Option<String> {
    match message {
        AgentMessage::User(user) => {
            let text = match &user.content {
                rpi_ai::types::UserContent::Text(text) => text.clone(),
                rpi_ai::types::UserContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        rpi_ai::types::UserContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            Some(text)
        }
        AgentMessage::Assistant(assistant) => {
            let text = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    rpi_ai::types::AssistantContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            Some(text)
        }
        AgentMessage::ToolResult(tool_result) => {
            let text = tool_result
                .content
                .iter()
                .filter_map(|block| match block {
                    rpi_ai::types::ToolResultContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            Some(text)
        }
        _ => None,
    }
}

/// `getMessageActivityTime` (session-manager.ts:673-684).
fn message_activity_time(entry: &MessageEntry) -> Option<i64> {
    match &entry.message {
        AgentMessage::User(user) => Some(user.timestamp),
        AgentMessage::Assistant(assistant) => Some(assistant.timestamp),
        _ => parse_iso8601_ms(&entry.timestamp),
    }
}

/// `buildSessionInfo` (session-manager.ts:687-766): any read/parse failure
/// yields `None` (the file is skipped).
pub fn build_session_info(file_path: &Path) -> Option<SessionInfo> {
    let stats = std::fs::metadata(file_path).ok()?;
    let mtime_ms = stats
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let content = std::fs::read_to_string(file_path).ok()?;
    let records = parse_session_entries(&content);

    let mut header: Option<SessionHeader> = None;
    let mut message_count = 0u64;
    let mut first_message = String::new();
    let mut all_messages: Vec<String> = Vec::new();
    let mut name: Option<String> = None;
    let mut last_activity_time: Option<i64> = None;

    for record in &records {
        let Some(typed) = record.typed() else {
            continue;
        };
        // First typed entry must be the header (session-manager.ts:705-709).
        if header.is_none() {
            if let FileEntry::Session(parsed) = typed {
                header = Some(parsed.clone());
                continue;
            }
            return None;
        }

        match typed {
            FileEntry::SessionInfo(session_info) => {
                // Latest session_info wins, including explicit clears.
                name = session_info
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned);
            }
            FileEntry::Message(message_entry) => {
                message_count += 1;
                if let Some(activity) = message_activity_time(message_entry) {
                    last_activity_time =
                        Some(last_activity_time.map_or(activity, |last: i64| last.max(activity)));
                }
                let is_user = matches!(message_entry.message, AgentMessage::User(_));
                let is_assistant = matches!(message_entry.message, AgentMessage::Assistant(_));
                if !is_user && !is_assistant {
                    continue;
                }
                let Some(text) = extract_message_text(&message_entry.message) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                all_messages.push(text.clone());
                if first_message.is_empty() && is_user {
                    first_message = text;
                }
            }
            _ => {}
        }
    }

    let header = header?;
    let header_time = parse_iso8601_ms(&header.timestamp);
    let modified_ms = match last_activity_time {
        Some(activity) if activity > 0 => activity,
        _ => header_time.unwrap_or(mtime_ms),
    };

    Some(SessionInfo {
        path: file_path.to_path_buf(),
        id: header.id.clone(),
        cwd: header.cwd.clone(),
        name,
        parent_session_path: header.parent_session.clone(),
        created_ms: header_time.unwrap_or(0),
        modified_ms,
        message_count,
        first_message: if first_message.is_empty() {
            "(no messages)".to_owned()
        } else {
            first_message
        },
        all_messages_text: all_messages.join(" "),
    })
}

/// `listSessionsFromDir` (session-manager.ts:1589-1625).
fn list_sessions_from_dir(dir: &Path) -> Vec<SessionInfo> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            if let Some(info) = build_session_info(&path) {
                sessions.push(info);
            }
        }
    }
    sessions
}

impl SessionManager {
    /// `SessionManager.list` (session-manager.ts:1638-1651).
    pub fn list(cwd: &Path, session_dir: Option<&Path>) -> Vec<SessionInfo> {
        let dir = match session_dir {
            Some(dir) => PathBuf::from(normalize_path(&dir.to_string_lossy())),
            None => match get_default_session_dir(cwd) {
                Ok(dir) => dir,
                Err(_) => return Vec::new(),
            },
        };
        let filter_cwd = session_dir.is_some() && dir != get_default_session_dir_path(cwd, None);
        let resolved_cwd = resolve_path(
            &cwd.to_string_lossy(),
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        );
        let mut sessions: Vec<SessionInfo> = list_sessions_from_dir(&dir)
            .into_iter()
            .filter(|session| {
                !filter_cwd || session_cwd_matches(Some(session.cwd.as_str()), &resolved_cwd)
            })
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.modified_ms));
        sessions
    }

    /// `SessionManager.listAll` (session-manager.ts:1653-1711).
    pub fn list_all(session_dir: Option<&Path>) -> Vec<SessionInfo> {
        if let Some(custom_dir) = session_dir {
            let dir = PathBuf::from(normalize_path(&custom_dir.to_string_lossy()));
            let mut sessions = list_sessions_from_dir(&dir);
            sessions.sort_by_key(|s| std::cmp::Reverse(s.modified_ms));
            return sessions;
        }

        let sessions_dir = crate::config::get_sessions_dir();
        let entries = match std::fs::read_dir(&sessions_dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut sessions = Vec::new();
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                sessions.extend(list_sessions_from_dir(&entry.path()));
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.modified_ms));
        sessions
    }
}
