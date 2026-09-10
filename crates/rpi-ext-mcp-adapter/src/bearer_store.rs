//! URL-bound static bearer tokens in the OS credential store
//! (R7.2.11.3 / #366, `mcp-bearer-store.ts` @ 10a45367).
//!
//! `bearerTokenStore: true` on a server entry opts the `auth: "bearer"`
//! resolution chain into an OS-store lookup keyed by the SERVER NAME; the
//! stored record binds the token to the server URL
//! (`{token, serverUrl}`), and a URL mismatch yields no token (the
//! credential for endpoint A is never shipped to endpoint B — same
//! binding rule as `merge_server_maps`' URL-bound auth fields).
//!
//! Account naming, record shape and the >1000-char manifest+chunk split
//! are byte-for-byte identical to the upstream so format-level
//! interoperability holds. Service name: `rpi-mcp-adapter.bearer`
//! [VARIANT — upstream `pi-mcp-adapter.bearer`, brand rename precedent
//! `rpi-mcp-adapter.oauth`].
//!
//! The upstream `pi-mcp-adapter token set|status|remove <server>` CLI is
//! NOT ported: the host has no extension CLI subcommand channel (candidate
//! ABI gap, extension-abi.md §8.5); tokens reach the store via hosts that
//! expose a keyring-compatible CLI or a future additive host call.
//!
//! Security (G4): token values never reach tracing or spill files; the
//! record type carries no `Debug`.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::AdapterError;
use crate::oauth::store::SecretStore;

/// `BEARER_SECRET_SERVICE` (mcp-bearer-store.ts:29) — product rename
/// [VARIANT].
pub const BEARER_SECRET_SERVICE: &str = "rpi-mcp-adapter.bearer";
/// `BEARER_SECRET_CHUNK_SIZE` (mcp-bearer-store.ts:31).
const BEARER_SECRET_CHUNK_SIZE: usize = 1000;

/// `BearerCredentialStatus` (mcp-bearer-store.ts:52-57).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BearerCredentialStatus {
    Present,
    Missing,
    UrlMismatch,
    Unavailable { message: String },
}

/// Map a store error to the upstream's user-facing availability message
/// (`inspectBearerTokenForUrl` catch arm).
fn unavailable(message: &str) -> BearerCredentialStatus {
    BearerCredentialStatus::Unavailable {
        message: format!(
            "Bearer token secure credential store unavailable. \
             Configure or unlock the OS credential store and retry. ({message})"
        ),
    }
}

/// `getBearerAccount` (mcp-bearer-store.ts:210-215): `sha256-<hex(name)>`.
fn bearer_account(server_name: &str) -> Result<String, AdapterError> {
    if server_name.is_empty() {
        return Err(AdapterError::InvalidConfigValue(
            "Invalid MCP server name: \"\"".to_string(),
        ));
    }
    let digest = Sha256::digest(server_name.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("sha256-{hex}"))
}

/// `isBearerChunkManifest` (mcp-bearer-store.ts:235-245).
fn parse_chunk_manifest(payload: &str) -> Option<(usize, String)> {
    let value: Value = serde_json::from_str(payload).ok()?;
    if value.get("__piMcpAdapterBearerChunked") != Some(&json!(1)) {
        return None;
    }
    let chunk_count = value.get("chunkCount")?.as_u64()? as usize;
    if chunk_count == 0 {
        return None;
    }
    let chunk_digest = value.get("chunkDigest")?.as_str()?.to_string();
    if chunk_digest.len() != 16 || !chunk_digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((chunk_count, chunk_digest))
}

fn chunk_account(account: &str, digest: &str, index: usize) -> String {
    format!("{account}.chunk.{digest}.{index}")
}

fn create_manifest(payload: &str) -> (usize, String) {
    let digest = Sha256::digest(payload.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    (payload.len().div_ceil(BEARER_SECRET_CHUNK_SIZE), hex)
}

/// `readBearerRecordFromStore` (mcp-bearer-store.ts:255-288): single entry
/// or chunk-manifest reassembly, then `{token, serverUrl}` shape check.
fn read_record(
    store: &dyn SecretStore,
    server_name: &str,
) -> Result<Option<(String, String)>, AdapterError> {
    let account = bearer_account(server_name)?;
    let payload = store
        .read(&account)
        .ok_or_else(|| AdapterError::InvalidConfigValue("no bearer record".to_string()))
        .ok();
    let Some(payload) = payload else {
        return Ok(None);
    };
    let record_payload = match parse_chunk_manifest(&payload) {
        Some((count, digest)) => {
            let mut chunks: Vec<String> = Vec::with_capacity(count);
            for index in 0..count {
                let chunk = store
                    .read(&chunk_account(&account, &digest, index))
                    .ok_or_else(|| {
                        AdapterError::InvalidConfigValue("missing bearer chunk".to_string())
                    })?;
                chunks.push(chunk);
            }
            chunks.join("")
        }
        None => payload,
    };
    let parsed: Value = serde_json::from_str(&record_payload).map_err(|_| {
        AdapterError::InvalidConfigValue(format!(
            "Failed to parse stored bearer token record for {server_name}"
        ))
    })?;
    let token = parsed.get("token").and_then(Value::as_str);
    let server_url = parsed.get("serverUrl").and_then(Value::as_str);
    match (token, server_url) {
        (Some(token), Some(server_url)) => Ok(Some((token.to_string(), server_url.to_string()))),
        _ => Err(AdapterError::InvalidConfigValue(format!(
            "Stored bearer token record for {server_name} has invalid shape"
        ))),
    }
}

fn remove_chunks(store: &dyn SecretStore, account: &str, manifest: Option<(usize, String)>) {
    if let Some((count, digest)) = manifest {
        for index in 0..count {
            store.remove(&chunk_account(account, &digest, index));
        }
    }
}

/// `writeBearerRecordToStore` (mcp-bearer-store.ts:290-329): chunked write
/// with digest-distinct cleanup (an identical payload reuses the previous
/// digest's chunk accounts; a failed rewrite of a NEW digest never deletes
/// the still-installed previous credential).
fn write_record(
    store: &dyn SecretStore,
    server_name: &str,
    token: &str,
    server_url: &str,
) -> Result<(), AdapterError> {
    let account = bearer_account(server_name)?;
    let payload = serde_json::to_string(&json!({
        "token": token,
        "serverUrl": server_url,
    }))
    .unwrap_or_default();
    let previous_manifest = store.read(&account).and_then(|p| parse_chunk_manifest(&p));
    let manifest = if payload.len() > BEARER_SECRET_CHUNK_SIZE {
        Some(create_manifest(&payload))
    } else {
        None
    };
    let write_result: Result<(), AdapterError> = (|| {
        if let Some((count, digest)) = &manifest {
            for index in 0..*count {
                let start = index * BEARER_SECRET_CHUNK_SIZE;
                let end = ((index + 1) * BEARER_SECRET_CHUNK_SIZE).min(payload.len());
                store.write(
                    &chunk_account(&account, digest, index),
                    &payload[start..end],
                )?;
            }
            store.write(
                &account,
                &serde_json::to_string(&json!({
                    "__piMcpAdapterBearerChunked": 1,
                    "chunkCount": count,
                    "chunkDigest": digest,
                }))
                .unwrap_or_default(),
            )?;
        } else {
            store.write(&account, &payload)?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        // Cleanup only digest-distinct chunks (upstream's rollback rule).
        if previous_manifest.as_ref() != manifest.as_ref() {
            remove_chunks(store, &account, manifest);
        }
        return Err(AdapterError::InvalidConfigValue(
            "Failed to write bearer token to the OS secure credential store".to_string(),
        ));
    }
    if previous_manifest.as_ref().map(|m| m.1.clone()) != manifest.as_ref().map(|m| m.1.clone()) {
        remove_chunks(store, &account, previous_manifest);
    }
    Ok(())
}

/// `removeBearerRecordFromStore` (mcp-bearer-store.ts:331-343).
fn remove_record(store: &dyn SecretStore, server_name: &str) -> Result<(), AdapterError> {
    let account = bearer_account(server_name)?;
    let manifest = store.read(&account).and_then(|p| parse_chunk_manifest(&p));
    remove_chunks(store, &account, manifest);
    store.remove(&account);
    Ok(())
}

/// `getBearerTokenForUrl` (mcp-bearer-store.ts:345-349): URL-bound lookup —
/// a stored record for a different URL yields no token.
pub fn get_bearer_token_for_url(
    store: &dyn SecretStore,
    server_name: &str,
    server_url: &str,
) -> Option<String> {
    match read_record(store, server_name) {
        Ok(Some((token, stored_url))) if stored_url == server_url => Some(token),
        _ => None,
    }
}

/// `saveBearerTokenForUrl` (mcp-bearer-store.ts:351-353).
pub fn save_bearer_token_for_url(
    store: &dyn SecretStore,
    server_name: &str,
    token: &str,
    server_url: &str,
) -> Result<(), AdapterError> {
    write_record(store, server_name, token, server_url)
}

/// `removeBearerToken` (mcp-bearer-store.ts:355-357).
pub fn remove_bearer_token(store: &dyn SecretStore, server_name: &str) -> Result<(), AdapterError> {
    remove_record(store, server_name)
}

/// `inspectBearerTokenForUrl` (mcp-bearer-store.ts:359-371).
pub fn inspect_bearer_token_for_url(
    store: &dyn SecretStore,
    server_name: &str,
    server_url: &str,
) -> BearerCredentialStatus {
    match read_record(store, server_name) {
        Ok(None) => BearerCredentialStatus::Missing,
        Ok(Some((_, stored_url))) => {
            if stored_url == server_url {
                BearerCredentialStatus::Present
            } else {
                BearerCredentialStatus::UrlMismatch
            }
        }
        Err(error) => unavailable(&error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::store::MemorySecretStore;

    fn store() -> MemorySecretStore {
        MemorySecretStore::new()
    }

    #[test]
    fn account_is_sha256_of_server_name() {
        let digest = Sha256::digest(b"test-server");
        let expected = format!(
            "sha256-{}",
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert_eq!(
            bearer_account("test-server").as_deref().ok(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn roundtrip_url_bound() {
        let store = store();
        save_bearer_token_for_url(&store, "srv", "tok-1", "https://a.test/mcp").expect("save");
        assert_eq!(
            get_bearer_token_for_url(&store, "srv", "https://a.test/mcp"),
            Some("tok-1".to_string())
        );
        // URL mismatch → no token (credential never crosses endpoints).
        assert_eq!(
            get_bearer_token_for_url(&store, "srv", "https://evil.test/mcp"),
            None
        );
        assert_eq!(
            inspect_bearer_token_for_url(&store, "srv", "https://a.test/mcp"),
            BearerCredentialStatus::Present
        );
        assert_eq!(
            inspect_bearer_token_for_url(&store, "srv", "https://b.test/mcp"),
            BearerCredentialStatus::UrlMismatch
        );
        remove_bearer_token(&store, "srv").expect("remove");
        assert_eq!(
            inspect_bearer_token_for_url(&store, "srv", "https://a.test/mcp"),
            BearerCredentialStatus::Missing
        );
    }

    #[test]
    fn oversized_tokens_chunk_reassemble_and_cleanup() {
        let store = store();
        let long_token = "t".repeat(2_400);
        let url = "https://a.test/mcp";
        save_bearer_token_for_url(&store, "srv", &long_token, url).expect("save");
        // Manifest + 3 chunks exist under the sha256 account.
        let account = bearer_account("srv").expect("account");
        let manifest = store.read(&account).expect("manifest");
        assert!(manifest.contains("__piMcpAdapterBearerChunked"));
        assert_eq!(
            get_bearer_token_for_url(&store, "srv", url),
            Some(long_token.clone())
        );
        // A second save with a different token replaces the chunks (the old
        // digest's accounts are removed).
        save_bearer_token_for_url(&store, "srv", "short", url).expect("resave");
        assert_eq!(
            get_bearer_token_for_url(&store, "srv", url),
            Some("short".to_string())
        );
        // No stray chunk accounts remain (the manifest is gone).
        let manifest_after = store.read(&account).expect("short record");
        assert!(!manifest_after.contains("__piMcpAdapterBearerChunked"));
    }

    #[test]
    fn malformed_record_reports_unavailable_or_missing() {
        let store = store();
        // No record at all → Missing.
        assert_eq!(
            inspect_bearer_token_for_url(&store, "srv", "https://a.test/mcp"),
            BearerCredentialStatus::Missing
        );
        // A record with invalid JSON shape → Unavailable (read error path).
        let account = bearer_account("srv").expect("account");
        store.write(&account, "not json").expect("write raw");
        assert!(matches!(
            inspect_bearer_token_for_url(&store, "srv", "https://a.test/mcp"),
            BearerCredentialStatus::Unavailable { .. }
        ));
    }
}
