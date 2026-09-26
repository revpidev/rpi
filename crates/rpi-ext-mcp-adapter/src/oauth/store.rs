//! OAuth credential store: OS keyring access, >1000-char manifest+chunk
//! splitting, legacy `tokens.json` one-shot import (FR-P1-04, design §3.7).
//!
//! Port of `mcp-auth.ts` (storage half) @ 3d953f90. Fail closed: no plaintext
//! fallback when the OS credential store is unavailable.
//!
//! Service name: `rpi-mcp-adapter.oauth` [VARIANT — upstream uses
//! `pi-mcp-adapter.oauth`]; account naming and chunk format are byte-for-byte
//! identical to the upstream to preserve format-level interoperability.
//!
//! **Security**: resolved token values MUST NEVER reach tracing logs or spill
//! files (G4 red line). The `AuthEntry` type deliberately has no `Debug`
//! impl that includes token text.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::AdapterError;
use crate::oauth::encrypted_store;

/// `AUTH_SECRET_SERVICE` (mcp-auth.ts:23) — product-name rename [VARIANT].
pub const AUTH_SECRET_SERVICE: &str = "rpi-mcp-adapter.oauth";

/// `AUTH_SECRET_CHUNK_SIZE` (mcp-auth.ts:32).
const AUTH_SECRET_CHUNK_SIZE: usize = 1000;

/// `AUTH_CHUNK_MANIFEST_KEY` (mcp-auth.ts:42).
const AUTH_CHUNK_MANIFEST_KEY: &str = "__piMcpAdapterOAuthChunked";

/// `StoredTokens` (mcp-auth.ts:45-52). Manual `Debug`: token values are
/// redacted — a stray `tracing::debug!("{:?}")` must never leak them (G4).
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}

impl std::fmt::Debug for StoredTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredTokens")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("issuer", &self.issuer)
            .finish()
    }
}

/// `StoredClientInfo` (mcp-auth.ts:55-71). Manual `Debug`: `client_secret`
/// is redacted (client_id is not credential material for logging purposes).
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct StoredClientInfo {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uris: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_pre_registered: Option<bool>,
}

impl std::fmt::Debug for StoredClientInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredClientInfo")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("client_id_issued_at", &self.client_id_issued_at)
            .field("client_secret_expires_at", &self.client_secret_expires_at)
            .field("redirect_uris", &self.redirect_uris)
            .field("issuer", &self.issuer)
            .field("config_pre_registered", &self.config_pre_registered)
            .finish()
    }
}

/// `AuthEntry` (mcp-auth.ts:74-80). Serialized as JSON with camelCase keys
/// to match the upstream wire format. Manual `Debug`: the PKCE code
/// verifier is redacted alongside the nested token/client secrets.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<StoredTokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<StoredClientInfo>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "codeVerifier")]
    pub code_verifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "oauthState")]
    pub oauth_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "serverUrl")]
    pub server_url: Option<String>,
}

impl std::fmt::Debug for AuthEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthEntry")
            .field("tokens", &self.tokens)
            .field("client_info", &self.client_info)
            .field("code_verifier", &"<redacted>")
            .field("oauth_state", &"<redacted>")
            .field("server_url", &self.server_url)
            .finish()
    }
}

/// `AuthEntryChunkManifest` (mcp-auth.ts:143-147).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChunkManifest {
    #[serde(rename = "__piMcpAdapterOAuthChunked")]
    marker: u8,
    chunk_count: usize,
    chunk_digest: String,
}

/// `AuthStorageOptions` (mcp-auth.ts:82-85 + #580 `credentialStore`).
#[derive(Debug, Clone, Default)]
pub struct AuthStorageOptions {
    pub base_dir: Option<PathBuf>,
    /// `settings.oauthCredentialStore` selection (#580): `Some(_)` builds
    /// the encrypted-file backend instead of the OS keyring.
    pub credential_store: Option<CredentialStoreSelection>,
}

/// Which concrete backend a store is (`AuthSecretStore.kind`, #580).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStoreKind {
    /// The OS secure credential store (keyring).
    OsSecureCredentialStore,
    /// The encrypted-file backend (`settings.oauthCredentialStore:
    /// "encrypted-file"`, #580).
    EncryptedFile,
}

/// `credentialStore` selection (`AuthStorageOptions.credentialStore`,
/// mcp-auth.ts:129-133 @ 97435aab).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStoreSelection {
    EncryptedFile,
}

impl CredentialStoreSelection {
    /// Parse `settings.oauthCredentialStore` ("encrypted-file"; anything
    /// else — including a wrongly-typed value — keeps the default keyring).
    pub fn from_settings(settings: Option<&serde_json::Map<String, Value>>) -> Option<Self> {
        let value = settings?.get("oauthCredentialStore")?.as_str()?;
        (value == "encrypted-file").then_some(CredentialStoreSelection::EncryptedFile)
    }
}

/// Trait abstracting the secret store so tests can inject a mock without
/// touching the real OS keyring (design §5.5: `keyring` crate mock
/// credential store feature — implemented as an in-memory backend here).
/// Operations are fallible (`#580`): backend failures surface as
/// [`AdapterError::OAuthCredentialStoreUnavailable`] classified errors
/// instead of silently reading as "absent".
pub trait SecretStore: Send + Sync {
    fn read(&self, account: &str) -> Result<Option<String>, AdapterError>;
    fn write(&self, account: &str, payload: &str) -> Result<(), AdapterError>;
    fn remove(&self, account: &str) -> Result<(), AdapterError>;
    /// `store.kind` (mcp-auth.ts:196): the encrypted-file backend never
    /// chunks and never runs the legacy plaintext import.
    fn kind(&self) -> SecretStoreKind {
        SecretStoreKind::OsSecureCredentialStore
    }
    /// Whether this backend imposes the small per-value ceiling that makes
    /// chunking necessary regardless of platform (`sizeLimitedAuthSecretStore`,
    /// #565: `PI_MCP_ADAPTER_TEST_AUTH_STORE === "sizelimited"`).
    fn forces_chunking(&self) -> bool {
        false
    }
}

/// In-memory secret store for tests (upstream `memoryAuthSecretStore`).
pub struct MemorySecretStore {
    entries: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
}

impl Clone for MemorySecretStore {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl Default for MemorySecretStore {
    fn default() -> Self {
        Self {
            entries: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemorySecretStore {
    fn read(&self, account: &str) -> Result<Option<String>, AdapterError> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .cloned())
    }

    fn write(&self, account: &str, payload: &str) -> Result<(), AdapterError> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), payload.to_string());
        Ok(())
    }

    fn remove(&self, account: &str) -> Result<(), AdapterError> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
        Ok(())
    }
}

/// Size-limited store that mimics the Windows Credential Manager per-value
/// ceiling (upstream `sizeLimitedAuthSecretStore`; #565: also the
/// platform-independent chunking stand-in via `forces_chunking`).
#[derive(Clone, Default)]
pub struct SizeLimitedSecretStore {
    entries: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
}

/// `AUTH_SECRET_VALUE_LIMIT` (mcp-auth.ts:34).
const AUTH_SECRET_VALUE_LIMIT: usize = 1280;

impl SizeLimitedSecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for SizeLimitedSecretStore {
    fn read(&self, account: &str) -> Result<Option<String>, AdapterError> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .cloned())
    }

    fn write(&self, account: &str, payload: &str) -> Result<(), AdapterError> {
        if payload.len() > AUTH_SECRET_VALUE_LIMIT {
            return Err(AdapterError::InvalidConfigValue(format!(
                "Value exceeds the platform limit of {} chars",
                AUTH_SECRET_VALUE_LIMIT
            )));
        }
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), payload.to_string());
        Ok(())
    }

    fn remove(&self, account: &str) -> Result<(), AdapterError> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
        Ok(())
    }

    fn forces_chunking(&self) -> bool {
        true
    }
}

/// An unavailable store that always errors (for fail-closed testing).
pub struct UnavailableSecretStore;

impl SecretStore for UnavailableSecretStore {
    fn read(&self, _account: &str) -> Result<Option<String>, AdapterError> {
        Err(encrypted_store::credential_store_unavailable(
            "read",
            false,
            "simulated secure credential store unavailable".to_string(),
        ))
    }

    fn write(&self, _account: &str, _payload: &str) -> Result<(), AdapterError> {
        Err(encrypted_store::credential_store_unavailable(
            "write",
            false,
            "simulated secure credential store unavailable".to_string(),
        ))
    }

    fn remove(&self, _account: &str) -> Result<(), AdapterError> {
        Err(encrypted_store::credential_store_unavailable(
            "remove",
            false,
            "simulated secure credential store unavailable".to_string(),
        ))
    }
}

/// `getAuthEntryAccount` (mcp-auth.ts:433-438): `sha256-<hex>`.
pub fn get_auth_entry_account(server_name: &str) -> String {
    let digest = Sha256::digest(server_name.as_bytes());
    format!("sha256-{}", hex::encode(&digest))
}

/// `hex::encode` — use `sha2`'s output formatted as lowercase hex.
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// `getAuthBaseDir` (mcp-auth.ts:416-420).
pub fn get_auth_base_dir(options: &AuthStorageOptions) -> PathBuf {
    if let Ok(override_dir) = std::env::var("MCP_OAUTH_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Some(base) = &options.base_dir {
        return base.clone();
    }
    // Agent dir: RPI_CODING_AGENT_DIR → ~/.rpi/agent (ADR-0001)
    let agent_dir = std::env::var("RPI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            crate::utils::home_dir()
                .map(|h| h.join(".rpi").join("agent"))
                .unwrap_or_else(|| PathBuf::from(".rpi").join("agent"))
        });
    agent_dir.join("mcp-oauth")
}

/// `getServerDir` (mcp-auth.ts:425-431).
fn get_server_dir(server_name: &str, options: &AuthStorageOptions) -> PathBuf {
    get_auth_base_dir(options).join(get_auth_entry_account(server_name))
}

/// `getAuthEntryFilePath` (mcp-auth.ts:443-445).
pub fn get_auth_entry_file_path(server_name: &str, options: &AuthStorageOptions) -> PathBuf {
    get_server_dir(server_name, options).join("tokens.json")
}

/// `createChunkManifest` (mcp-auth.ts:599-605 → #565 form: chunk count
/// comes from the actual split, digest from the joined payload).
fn create_chunk_manifest(payload: &str, chunk_count: usize) -> ChunkManifest {
    let digest = Sha256::digest(payload.as_bytes());
    let hex_digest: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    ChunkManifest {
        marker: 1,
        chunk_count,
        chunk_digest: hex_digest,
    }
}

/// `getAuthEntryChunkDigest` (mcp-auth.ts:795-797).
fn get_auth_entry_chunk_digest(payload: &str) -> String {
    let digest = Sha256::digest(payload.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// `shouldChunkAuthPayload` (mcp-auth.ts:791-793 @ 97435aab / #565):
/// chunking is a Windows Credential Manager workaround — it only applies
/// on Windows or when the backend forces it (the size-limited test store);
/// the encrypted-file backend never chunks.
///
/// Size note (deliberate, upstream-shared): the threshold and the split
/// count CHARACTERS (`payload.length` in JS = UTF-16 units upstream,
/// `chars()` here), not bytes — a payload of CJK characters near the
/// threshold can exceed the Windows byte limit at the margin. Matching
/// the upstream unit keeps the chunk layout byte-identical for
/// cross-implementation reads; do not switch to byte-based splitting
/// without an upstream-format decision.
fn should_chunk_auth_payload(store: &dyn SecretStore, payload: &str) -> bool {
    store.kind() != SecretStoreKind::EncryptedFile
        && payload.chars().count() > AUTH_SECRET_CHUNK_SIZE
        && (cfg!(windows) || store.forces_chunking())
}

/// `splitAuthPayload` (mcp-auth.ts:799-810): fixed-size chunks that rejoin
/// to the exact payload. Upstream splits JS UTF-16 code units with a
/// surrogate-pair guard; the Rust port splits by characters (code points)
/// at `AUTH_SECRET_CHUNK_SIZE` — the chunks are only ever re-read by this
/// same store (round-trip integrity is pinned by the digest), so the unit
/// difference is not observable through the store API.
fn split_auth_payload(payload: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut remaining = payload;
    while !remaining.is_empty() {
        let take = remaining
            .char_indices()
            .nth(AUTH_SECRET_CHUNK_SIZE)
            .map(|(offset, _)| offset)
            .unwrap_or(remaining.len());
        let (chunk, rest) = remaining.split_at(take);
        chunks.push(chunk);
        remaining = rest;
    }
    chunks
}

/// `getAuthEntryChunkAccount` (mcp-auth.ts:562-564).
fn get_chunk_account(account: &str, manifest: &ChunkManifest, index: usize) -> String {
    format!("{account}.chunk.{}.{}", manifest.chunk_digest, index)
}

/// `getAuthEntryChunkAccounts` (mcp-auth.ts:566-568).
fn get_chunk_accounts(account: &str, manifest: &ChunkManifest) -> Vec<String> {
    (0..manifest.chunk_count)
        .map(|i| get_chunk_account(account, manifest, i))
        .collect()
}

/// `isAuthEntryChunkManifest` (mcp-auth.ts:551-559).
fn parse_chunk_manifest(payload: &str) -> Option<ChunkManifest> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let obj = value.as_object()?;
    if obj.get(AUTH_CHUNK_MANIFEST_KEY) != Some(&json!(1)) {
        return None;
    }
    let chunk_count = obj.get("chunkCount")?.as_u64()? as usize;
    if chunk_count == 0 {
        return None;
    }
    let chunk_digest = obj.get("chunkDigest")?.as_str()?;
    if chunk_digest.len() != 16 || !chunk_digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(ChunkManifest {
        marker: 1,
        chunk_count,
        chunk_digest: chunk_digest.to_string(),
    })
}

/// `readChunkedAuthEntry` (mcp-auth.ts:607-630 @ 97435aab): join the
/// digest-addressed chunks and verify the integrity digest (#565) — a
/// corrupted or partial chunk set fails closed instead of parsing a
/// truncated payload.
fn read_chunked_entry(
    store: &dyn SecretStore,
    server_name: &str,
    account: &str,
    manifest: &ChunkManifest,
) -> Result<AuthEntry, AdapterError> {
    let mut chunks = Vec::new();
    for chunk_account in get_chunk_accounts(account, manifest) {
        match store.read(&chunk_account)? {
            Some(chunk) => chunks.push(chunk),
            None => {
                return Err(encrypted_store::credential_store_unavailable(
                    "read",
                    store.kind() == SecretStoreKind::EncryptedFile,
                    format!("Missing OAuth credential chunk {chunk_account} for {server_name}"),
                ));
            }
        }
    }
    let payload = chunks.join("");
    if get_auth_entry_chunk_digest(&payload) != manifest.chunk_digest {
        return Err(encrypted_store::credential_store_unavailable(
            "read",
            store.kind() == SecretStoreKind::EncryptedFile,
            "OAuth credential chunk integrity check failed".to_string(),
        ));
    }
    let label = auth_secret_store_label(store);
    parse_auth_entry_payload(server_name, &payload, &format!("{label} chunks"))
}

/// `authSecretStoreLabel` (mcp-auth.ts:201-203).
fn auth_secret_store_label(store: &dyn SecretStore) -> &'static str {
    if store.kind() == SecretStoreKind::EncryptedFile {
        "encrypted OAuth credential file store"
    } else {
        "OS secure credential store"
    }
}

/// `readLegacyAuthEntry` (mcp-auth.ts:626-631).
fn read_legacy_entry(server_name: &str, options: &AuthStorageOptions) -> Option<AuthEntry> {
    let file_path = get_auth_entry_file_path(server_name, options);
    let data = std::fs::read_to_string(&file_path).ok()?;
    parse_auth_entry_payload(server_name, &data, &file_path.to_string_lossy()).ok()
}

/// `removeLegacyAuthEntry` (mcp-auth.ts:633-648).
fn remove_legacy_entry(server_name: &str, options: &AuthStorageOptions) {
    let file_path = get_auth_entry_file_path(server_name, options);
    if !file_path.exists() {
        return;
    }
    let _ = std::fs::remove_file(&file_path);
    let dir = get_server_dir(server_name, options);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `readExistingChunkManifest` (mcp-auth.ts:767-772).
fn read_existing_chunk_manifest(store: &dyn SecretStore, account: &str) -> Option<ChunkManifest> {
    let payload = store.read(account).ok()?;
    let payload = payload.as_deref()?;
    parse_chunk_manifest(payload)
}

/// `tryRemoveChunkPayloads` (mcp-auth.ts:782-789): stale chunk cleanup must
/// not hide a successful credential write.
fn try_remove_chunk_payloads(
    store: &dyn SecretStore,
    account: &str,
    manifest: Option<&ChunkManifest>,
) {
    if let Some(manifest) = manifest {
        for chunk_account in get_chunk_accounts(account, manifest) {
            let _ = store.remove(&chunk_account);
        }
    }
}

/// `writeSecureAuthEntryToStore` (mcp-auth.ts:809-842 @ 97435aab): chunk
/// when the backend needs it (#565 platform gate), compact otherwise
/// (multiline secrets corrupt gnome-keyring plaintext collections), clean
/// stale chunks when the digest changed, and fail closed (removing the
/// just-written chunks) on any write error.
fn write_secure_entry(
    store: &dyn SecretStore,
    server_name: &str,
    entry: &AuthEntry,
) -> Result<(), AdapterError> {
    let account = get_auth_entry_account(server_name);
    let payload = serialize_auth_entry(entry);

    let previous_manifest = read_existing_chunk_manifest(store, &account);
    let chunks = if should_chunk_auth_payload(store, &payload) {
        Some(split_auth_payload(&payload))
    } else {
        None
    };
    let manifest = chunks
        .as_ref()
        .map(|chunks| create_chunk_manifest(&payload, chunks.len()));

    let write_result = (|| -> Result<(), AdapterError> {
        if let (Some(manifest), Some(chunks)) = (&manifest, &chunks) {
            for (index, chunk) in chunks.iter().enumerate() {
                let chunk_account = get_chunk_account(&account, manifest, index);
                store.write(&chunk_account, chunk)?;
            }
            let manifest_json = serde_json::to_string(manifest).unwrap_or_default();
            store.write(&account, &manifest_json)?;
        } else {
            store.write(&account, &payload)?;
        }
        if previous_manifest.as_ref().map(|m| &m.chunk_digest)
            != manifest.as_ref().map(|m| &m.chunk_digest)
        {
            try_remove_chunk_payloads(store, &account, previous_manifest.as_ref());
        }
        Ok(())
    })();

    match write_result {
        Ok(()) => Ok(()),
        Err(error) => {
            try_remove_chunk_payloads(store, &account, manifest.as_ref());
            let label = auth_secret_store_label(store);
            Err(match error {
                AdapterError::OAuthCredentialStoreUnavailable { .. } => {
                    AdapterError::InvalidConfigValue(format!(
                        "Failed to write OAuth credentials for {server_name} to the {label}: {error}"
                    ))
                }
                other => AdapterError::InvalidConfigValue(format!(
                    "Failed to write OAuth credentials for {server_name} to the {label}: {other}"
                )),
            })
        }
    }
}

/// Serialize an AuthEntry to JSON matching the upstream wire format.
fn serialize_auth_entry(entry: &AuthEntry) -> String {
    serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string())
}

/// `parseAuthEntryPayload` (mcp-auth.ts:455-462).
fn parse_auth_entry_payload(
    server_name: &str,
    payload: &str,
    source: &str,
) -> Result<AuthEntry, AdapterError> {
    serde_json::from_str::<AuthEntry>(payload).map_err(|e| {
        AdapterError::InvalidConfigValue(format!(
            "Failed to parse OAuth credentials for {server_name} from {source}: {e}"
        ))
    })
}

/// `readAuthEntryFromStore` (mcp-auth.ts:751-786 @ 97435aab): manifest
/// reads verify the digest and compact on the next non-encrypted read when
/// the payload no longer needs chunking (#565); the encrypted-file backend
/// never chunks and never runs the legacy plaintext import (#580).
fn read_entry_from_store(
    store: &dyn SecretStore,
    server_name: &str,
    options: &AuthStorageOptions,
    migrate_legacy: bool,
) -> Result<Option<AuthEntry>, AdapterError> {
    let account = get_auth_entry_account(server_name);
    let payload = store.read(&account).map_err(|error| {
        let label = auth_secret_store_label(store);
        AdapterError::InvalidConfigValue(format!(
            "Failed to read OAuth credentials for {server_name} from the {label}: {error}"
        ))
    })?;

    if let Some(payload) = payload {
        let manifest = if store.kind() == SecretStoreKind::EncryptedFile {
            None
        } else {
            parse_chunk_manifest(&payload)
        };
        let entry = match &manifest {
            Some(manifest) => read_chunked_entry(store, server_name, &account, manifest)?,
            None => {
                let label = auth_secret_store_label(store);
                parse_auth_entry_payload(server_name, &payload, label)?
            }
        };
        if store.kind() != SecretStoreKind::EncryptedFile {
            remove_legacy_entry(server_name, options);
            // #565 compaction: an existing chunked record whose payload no
            // longer needs chunking is rewritten as a single entry.
            if manifest.is_some()
                && migrate_legacy
                && !should_chunk_auth_payload(store, &serialize_auth_entry(&entry))
            {
                write_secure_entry(store, server_name, &entry)?;
            }
        }
        return Ok(Some(entry));
    }

    if store.kind() == SecretStoreKind::EncryptedFile {
        return Ok(None);
    }

    // Try legacy plaintext import
    if let Some(legacy_entry) = read_legacy_entry(server_name, options) {
        if !migrate_legacy {
            return Ok(Some(legacy_entry));
        }
        // Migrate to secure store
        write_secure_entry(store, server_name, &legacy_entry)?;
        remove_legacy_entry(server_name, options);
        return Ok(Some(legacy_entry));
    }

    Ok(None)
}

/// The public credential store interface — wraps a `SecretStore` backend.
pub struct OAuthCredentialStore {
    backend: Box<dyn SecretStore>,
    options: AuthStorageOptions,
}

impl OAuthCredentialStore {
    /// Create the production store: the encrypted-file backend when
    /// `settings.oauthCredentialStore` selected it (#580), else the OS
    /// keyring (`getAuthSecretStore`, mcp-auth.ts:430-440).
    pub fn new(options: AuthStorageOptions) -> Self {
        let backend: Box<dyn SecretStore> =
            if options.credential_store == Some(CredentialStoreSelection::EncryptedFile) {
                Box::new(encrypted_store::EncryptedFileSecretStore::new())
            } else {
                Box::new(KeyringBackend::new())
            };
        Self { backend, options }
    }

    /// Create a store backed by a test-provided in-memory backend.
    pub fn with_backend(backend: Box<dyn SecretStore>, options: AuthStorageOptions) -> Self {
        Self { backend, options }
    }

    /// `getAuthEntry` (mcp-auth.ts:768-769).
    pub fn get_entry(&self, server_name: &str) -> Result<Option<AuthEntry>, AdapterError> {
        read_entry_from_store(self.backend.as_ref(), server_name, &self.options, true)
    }

    /// A status-shaped read (`hasStoredTokens`/`isTokenExpired` path,
    /// mcp-auth.ts readAuthEntry `{ migrateLegacy: false }`): no legacy
    /// import, no compaction — reads never mutate.
    pub fn get_entry_status(&self, server_name: &str) -> Result<Option<AuthEntry>, AdapterError> {
        read_entry_from_store(self.backend.as_ref(), server_name, &self.options, false)
    }

    /// `getAuthForUrl` (mcp-auth.ts:776-787).
    pub fn get_for_url(
        &self,
        server_name: &str,
        server_url: &str,
    ) -> Result<Option<AuthEntry>, AdapterError> {
        let entry = self.get_entry(server_name)?;
        match entry {
            None => Ok(None),
            Some(e) => {
                if e.server_url.as_deref() != Some(server_url) {
                    return Ok(None);
                }
                Ok(Some(e))
            }
        }
    }

    /// `saveAuthEntry` (mcp-auth.ts:812-819).
    pub fn save_entry(
        &self,
        server_name: &str,
        mut entry: AuthEntry,
        server_url: Option<&str>,
    ) -> Result<(), AdapterError> {
        if let Some(url) = server_url {
            entry.server_url = Some(url.to_string());
        }
        write_secure_entry(self.backend.as_ref(), server_name, &entry)?;
        remove_legacy_entry(server_name, &self.options);
        Ok(())
    }

    /// `removeAuthEntry` (mcp-auth.ts:840-849 @ 97435aab): chunk payloads
    /// are removed first, then the manifest/account entry; the legacy
    /// plaintext file follows.
    pub fn remove_entry(&self, server_name: &str) -> Result<(), AdapterError> {
        let account = get_auth_entry_account(server_name);
        if let Some(payload) = self.backend.read(&account)? {
            if let Some(manifest) = parse_chunk_manifest(&payload) {
                for chunk_account in get_chunk_accounts(&account, &manifest) {
                    self.backend.remove(&chunk_account)?;
                }
            }
        }
        self.backend.remove(&account)?;
        remove_legacy_entry(server_name, &self.options);
        Ok(())
    }

    /// `clearClientInfo` (mcp-auth.ts:983-989 @ 10a45367): drop the stored dynamic
    /// client registration while keeping tokens — a refresh-capable
    /// credential may still be usable, and the next interactive flow
    /// registers a fresh client (#503).
    pub fn clear_client_info(&self, server_name: &str) -> Result<(), AdapterError> {
        let Some(mut entry) = self.get_entry(server_name)? else {
            return Ok(());
        };
        entry.client_info = None;
        let server_url = entry.server_url.clone();
        self.save_entry(server_name, entry, server_url.as_deref())
    }

    /// `isTokenExpired` (mcp-auth.ts:958-963) — status read.
    pub fn is_token_expired(&self, server_name: &str) -> Result<Option<bool>, AdapterError> {
        let entry = self.get_entry_status(server_name)?;
        match entry.and_then(|e| e.tokens) {
            None => Ok(None),
            Some(tokens) => match tokens.expires_at {
                None => Ok(Some(false)),
                Some(exp) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    Ok(Some(exp < now))
                }
            },
        }
    }

    /// `hasStoredTokens` (mcp-auth.ts:968-971) — status read.
    pub fn has_stored_tokens(&self, server_name: &str) -> bool {
        self.get_entry_status(server_name)
            .ok()
            .flatten()
            .and_then(|e| e.tokens)
            .is_some()
    }

    /// `updateTokens` (mcp-auth.ts:861-875).
    pub fn update_tokens(
        &self,
        server_name: &str,
        tokens: StoredTokens,
        server_url: Option<&str>,
    ) -> Result<(), AdapterError> {
        let mut entry = self.get_entry(server_name)?.unwrap_or_default();
        if let Some(url) = server_url {
            if entry.server_url.as_deref() != Some(url) {
                entry.client_info = None;
                entry.code_verifier = None;
                entry.oauth_state = None;
            }
        }
        entry.tokens = Some(tokens);
        self.save_entry(server_name, entry, server_url)
    }

    /// `updateClientInfo` (mcp-auth.ts:880-894).
    pub fn update_client_info(
        &self,
        server_name: &str,
        client_info: StoredClientInfo,
        server_url: Option<&str>,
    ) -> Result<(), AdapterError> {
        let mut entry = self.get_entry(server_name)?.unwrap_or_default();
        if let Some(url) = server_url {
            if entry.server_url.as_deref() != Some(url) {
                entry.tokens = None;
                entry.code_verifier = None;
                entry.oauth_state = None;
            }
        }
        entry.client_info = Some(client_info);
        self.save_entry(server_name, entry, server_url)
    }

    /// `updateCodeVerifier` (mcp-auth.ts:899-908).
    pub fn update_code_verifier(
        &self,
        server_name: &str,
        code_verifier: String,
        server_url: Option<&str>,
    ) -> Result<(), AdapterError> {
        let mut entry = self.get_entry(server_name)?.unwrap_or_default();
        if let Some(url) = server_url {
            if entry.server_url.as_deref() != Some(url) {
                entry.tokens = None;
                entry.client_info = None;
                entry.oauth_state = None;
            }
        }
        entry.code_verifier = Some(code_verifier);
        self.save_entry(server_name, entry, server_url)
    }

    /// `updateOAuthState` (mcp-auth.ts:924-933).
    pub fn update_oauth_state(
        &self,
        server_name: &str,
        state: String,
        server_url: Option<&str>,
    ) -> Result<(), AdapterError> {
        let mut entry = self.get_entry(server_name)?.unwrap_or_default();
        if let Some(url) = server_url {
            if entry.server_url.as_deref() != Some(url) {
                entry.tokens = None;
                entry.client_info = None;
                entry.code_verifier = None;
            }
        }
        entry.oauth_state = Some(state);
        self.save_entry(server_name, entry, server_url)
    }
}

/// OS keyring backend using the `keyring` crate (upstream uses
/// `@napi-rs/keyring`, which is the napi binding to the same Rust
/// `keyring-rs` crate — semantics are identical).
struct KeyringBackend {
    service: String,
}

impl KeyringBackend {
    fn new() -> Self {
        Self {
            service: AUTH_SECRET_SERVICE.to_string(),
        }
    }
}

impl SecretStore for KeyringBackend {
    fn read(&self, account: &str) -> Result<Option<String>, AdapterError> {
        let entry = keyring::Entry::new(&self.service, account).map_err(|e| {
            encrypted_store::credential_store_unavailable(
                "read",
                false,
                format!("OAuth secure credential storage is unavailable: {e}"),
            )
        })?;
        match entry.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(other) => Err(encrypted_store::credential_store_unavailable(
                "read",
                false,
                format!("{other}"),
            )),
        }
    }

    fn write(&self, account: &str, payload: &str) -> Result<(), AdapterError> {
        let entry = keyring::Entry::new(&self.service, account).map_err(|e| {
            encrypted_store::credential_store_unavailable(
                "write",
                false,
                format!("OAuth secure credential storage is unavailable: {e}"),
            )
        })?;
        entry.set_password(payload).map_err(|e| {
            encrypted_store::credential_store_unavailable(
                "write",
                false,
                format!("Failed to write OAuth credentials to the OS credential store: {e}"),
            )
        })
    }

    fn remove(&self, account: &str) -> Result<(), AdapterError> {
        let entry = keyring::Entry::new(&self.service, account).map_err(|e| {
            encrypted_store::credential_store_unavailable(
                "remove",
                false,
                format!("OAuth secure credential storage is unavailable: {e}"),
            )
        })?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(encrypted_store::credential_store_unavailable(
                "remove",
                false,
                format!("Failed to remove OAuth credentials from the OS credential store: {e}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tokens() -> StoredTokens {
        StoredTokens {
            access_token: "access-123".to_string(),
            refresh_token: Some("refresh-456".to_string()),
            expires_at: Some(9999999999.0),
            scope: Some("read write".to_string()),
            issuer: None,
        }
    }

    #[test]
    fn account_is_sha256_of_server_name() {
        let account = get_auth_entry_account("test-server");
        assert!(account.starts_with("sha256-"));
        // Verify the hex matches a direct sha256 computation
        let expected = {
            let digest = sha2::Sha256::digest(b"test-server");
            format!(
                "sha256-{}",
                digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            )
        };
        assert_eq!(account, expected);
    }

    #[test]
    fn round_trip_small_entry() {
        let store = MemorySecretStore::new();
        let cred_store =
            OAuthCredentialStore::with_backend(Box::new(store), AuthStorageOptions::default());
        let entry = AuthEntry {
            tokens: Some(make_tokens()),
            server_url: Some("https://example.test/mcp".to_string()),
            ..Default::default()
        };
        cred_store.save_entry("test", entry.clone(), None).unwrap();

        let read = cred_store.get_entry("test").unwrap().expect("entry");
        assert_eq!(read.tokens.as_ref().unwrap().access_token, "access-123");
        assert_eq!(
            read.tokens.as_ref().unwrap().refresh_token.as_deref(),
            Some("refresh-456")
        );
        assert_eq!(read.server_url.as_deref(), Some("https://example.test/mcp"));
    }

    #[test]
    fn chunk_format_matches_upstream_layout() {
        // Build an entry whose JSON exceeds AUTH_SECRET_CHUNK_SIZE.
        let big_token = "x".repeat(AUTH_SECRET_CHUNK_SIZE * 2 + 100);
        let entry = AuthEntry {
            tokens: Some(StoredTokens {
                access_token: big_token,
                ..Default::default()
            }),
            ..Default::default()
        };

        // #565: chunking applies on Windows or with a size-limited backend
        // (upstream tests set PI_MCP_ADAPTER_TEST_AUTH_STORE=sizelimited);
        // the size-limited backend is the platform-independent stand-in.
        let store = SizeLimitedSecretStore::new();
        let cred_store = OAuthCredentialStore::with_backend(
            Box::new(store.clone()),
            AuthStorageOptions::default(),
        );
        cred_store.save_entry("big", entry, None).unwrap();

        // The account entry should be a manifest, not the raw payload.
        let account = get_auth_entry_account("big");
        let entries = store.entries.lock().unwrap();
        let manifest_payload = entries.get(&account).expect("manifest entry");
        let manifest: Value = serde_json::from_str(manifest_payload).unwrap();
        assert_eq!(manifest[AUTH_CHUNK_MANIFEST_KEY], json!(1));
        assert_eq!(manifest["chunkCount"], json!(3)); // ceil(2100/1000) = 3
        let digest = manifest["chunkDigest"].as_str().unwrap();
        assert_eq!(digest.len(), 16);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

        // Chunk accounts follow the `{account}.chunk.{digest}.{index}` format.
        for i in 0..3 {
            let chunk_account = format!("{account}.chunk.{digest}.{i}");
            assert!(entries.contains_key(&chunk_account), "missing chunk {i}");
        }

        drop(entries);

        // Round-trip: read back the full entry.
        let read = cred_store.get_entry("big").unwrap().expect("entry");
        assert_eq!(
            read.tokens.unwrap().access_token.len(),
            AUTH_SECRET_CHUNK_SIZE * 2 + 100
        );
    }

    #[test]
    fn url_change_invalidates_credentials() {
        let store = MemorySecretStore::new();
        let cred_store =
            OAuthCredentialStore::with_backend(Box::new(store), AuthStorageOptions::default());
        let entry = AuthEntry {
            tokens: Some(make_tokens()),
            server_url: Some("https://old.test/mcp".to_string()),
            ..Default::default()
        };
        cred_store.save_entry("srv", entry, None).unwrap();

        assert!(cred_store
            .get_for_url("srv", "https://old.test/mcp")
            .unwrap()
            .is_some());
        assert!(cred_store
            .get_for_url("srv", "https://new.test/mcp")
            .unwrap()
            .is_none());
    }

    #[test]
    fn token_expiry_detection() {
        let store = MemorySecretStore::new();
        let cred_store =
            OAuthCredentialStore::with_backend(Box::new(store), AuthStorageOptions::default());

        // No entry → None
        assert_eq!(cred_store.is_token_expired("srv").unwrap(), None);

        // Entry with future expiry → Some(false)
        cred_store
            .save_entry(
                "srv",
                AuthEntry {
                    tokens: Some(StoredTokens {
                        access_token: "tok".to_string(),
                        expires_at: Some(9999999999.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(cred_store.is_token_expired("srv").unwrap(), Some(false));

        // Entry with past expiry → Some(true)
        cred_store
            .save_entry(
                "srv",
                AuthEntry {
                    tokens: Some(StoredTokens {
                        access_token: "tok".to_string(),
                        expires_at: Some(1.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(cred_store.is_token_expired("srv").unwrap(), Some(true));
    }

    #[test]
    fn fail_closed_when_store_unavailable() {
        let cred_store = OAuthCredentialStore::with_backend(
            Box::new(UnavailableSecretStore),
            AuthStorageOptions::default(),
        );
        let entry = AuthEntry {
            tokens: Some(make_tokens()),
            ..Default::default()
        };
        // Write fails — no plaintext fallback.
        let result = cred_store.save_entry("srv", entry, None);
        assert!(result.is_err());
    }

    #[test]
    fn legacy_import_reads_and_deletes_plaintext() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-mcp-oauth-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let options = AuthStorageOptions {
            base_dir: Some(dir.clone()),
            credential_store: None,
        };
        let store = MemorySecretStore::new();

        // Write a legacy tokens.json
        let file_path = get_auth_entry_file_path("legacy-server", &options);
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        let entry = AuthEntry {
            tokens: Some(StoredTokens {
                access_token: "legacy-token".to_string(),
                ..Default::default()
            }),
            server_url: Some("https://legacy.test/mcp".to_string()),
            ..Default::default()
        };
        std::fs::write(&file_path, serde_json::to_string(&entry).unwrap()).unwrap();

        let cred_store = OAuthCredentialStore::with_backend(Box::new(store), options.clone());
        let read = cred_store
            .get_entry("legacy-server")
            .unwrap()
            .expect("entry");
        assert_eq!(read.tokens.unwrap().access_token, "legacy-token");

        // Legacy file should have been deleted after import.
        assert!(
            !file_path.exists(),
            "legacy file must be deleted after import"
        );

        // Second read comes from the secure store.
        let read2 = cred_store
            .get_entry("legacy-server")
            .unwrap()
            .expect("entry2");
        assert_eq!(read2.tokens.unwrap().access_token, "legacy-token");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_impls_redact_credentials() {
        let entry = AuthEntry {
            tokens: Some(make_tokens()),
            client_info: Some(StoredClientInfo {
                client_id: "client-id-1".to_string(),
                client_secret: Some("super-secret".to_string()),
                ..Default::default()
            }),
            code_verifier: Some("verifier-xyz".to_string()),
            oauth_state: Some("state-abc".to_string()),
            server_url: None,
        };
        let rendered = format!("{entry:?}");
        for secret in [
            "access-123",
            "refresh-456",
            "super-secret",
            "verifier-xyz",
            "state-abc",
        ] {
            assert!(
                !rendered.contains(secret),
                "leaked {secret} in Debug: {rendered}"
            );
        }
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("client-id-1")); // non-credential fields stay
    }

    #[test]
    fn remove_entry_cleans_chunks() {
        let big_token = "x".repeat(AUTH_SECRET_CHUNK_SIZE * 2);
        let entry = AuthEntry {
            tokens: Some(StoredTokens {
                access_token: big_token,
                ..Default::default()
            }),
            ..Default::default()
        };

        // #565: the size-limited backend forces chunking on every platform.
        let store = SizeLimitedSecretStore::new();
        let cred_store = OAuthCredentialStore::with_backend(
            Box::new(store.clone()),
            AuthStorageOptions::default(),
        );
        cred_store.save_entry("big", entry, None).unwrap();

        // Verify chunks exist
        let account = get_auth_entry_account("big");
        {
            let entries = store.entries.lock().unwrap();
            assert!(entries.len() > 1); // manifest + chunks
        }

        cred_store.remove_entry("big").unwrap();

        // All entries for this server should be gone
        {
            let entries = store.entries.lock().unwrap();
            for key in entries.keys() {
                if key.starts_with(&account) {
                    panic!("stale chunk remaining: {key}");
                }
            }
        }
    }

    /// A test backend whose chunking ceiling can flip (#565: the upstream
    /// sizelimited→memory env switch — both share one entries map there;
    /// here the flag is on the same underlying store).
    #[derive(Clone, Default)]
    struct FlipChunkingStore {
        entries: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
        force: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl SecretStore for FlipChunkingStore {
        fn read(&self, account: &str) -> Result<Option<String>, AdapterError> {
            Ok(self
                .entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(account)
                .cloned())
        }

        fn write(&self, account: &str, payload: &str) -> Result<(), AdapterError> {
            self.entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(account.to_string(), payload.to_string());
            Ok(())
        }

        fn remove(&self, account: &str) -> Result<(), AdapterError> {
            self.entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(account);
            Ok(())
        }

        fn forces_chunking(&self) -> bool {
            self.force.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn chunked_record_compacts_on_next_read_when_chunking_no_longer_needed() {
        // #565: write chunked while the backend forces chunking, then stop
        // forcing — the next read rewrites the record as a single entry and
        // removes the digest-addressed chunks.
        let backend = FlipChunkingStore::default();
        let big_token = "x".repeat(AUTH_SECRET_CHUNK_SIZE * 2);
        let entry = AuthEntry {
            tokens: Some(StoredTokens {
                access_token: big_token,
                ..Default::default()
            }),
            ..Default::default()
        };
        backend
            .force
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let cred_store = OAuthCredentialStore::with_backend(
            Box::new(backend.clone()),
            AuthStorageOptions::default(),
        );
        cred_store.save_entry("big", entry, None).unwrap();
        let account = get_auth_entry_account("big");
        {
            let entries = backend.entries.lock().unwrap();
            assert!(entries.keys().any(|k| k.contains(".chunk.")));
        }

        backend
            .force
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let read = cred_store.get_entry("big").unwrap().expect("entry");
        assert_eq!(
            read.tokens.unwrap().access_token.len(),
            AUTH_SECRET_CHUNK_SIZE * 2
        );
        {
            let entries = backend.entries.lock().unwrap();
            assert!(
                !entries.keys().any(|k| k.contains(".chunk.")),
                "chunks must be compacted away"
            );
            let payload = entries.get(&account).expect("single payload");
            assert!(!payload.contains(AUTH_CHUNK_MANIFEST_KEY));
        }
    }

    #[test]
    fn corrupted_chunk_fails_integrity_check() {
        // #565: a chunk set that no longer rejoins to the manifest digest
        // fails closed instead of parsing a truncated payload.
        let backend = FlipChunkingStore::default();
        backend
            .force
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let big_token = "x".repeat(AUTH_SECRET_CHUNK_SIZE * 2);
        let entry = AuthEntry {
            tokens: Some(StoredTokens {
                access_token: big_token,
                ..Default::default()
            }),
            ..Default::default()
        };
        let cred_store = OAuthCredentialStore::with_backend(
            Box::new(backend.clone()),
            AuthStorageOptions::default(),
        );
        cred_store.save_entry("big", entry, None).unwrap();
        {
            let mut entries = backend.entries.lock().unwrap();
            let chunk_key = entries
                .keys()
                .find(|k| k.contains(".chunk.0"))
                .expect("chunk 0")
                .clone();
            if let Some(chunk) = entries.get_mut(&chunk_key) {
                // Flip one character inside the chunk.
                chunk.replace_range(0..1, "y");
            }
        }
        let error = cred_store.get_entry("big").expect_err("integrity failure");
        assert!(error.to_string().contains("integrity check failed"));
    }
}
