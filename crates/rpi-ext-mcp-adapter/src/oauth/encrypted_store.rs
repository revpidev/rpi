//! Encrypted-file OAuth credential store (#580, `e4cd1c8`,
//! mcp-auth.ts:342-466 @ `97435aab`).
//!
//! An explicitly selected, externally keyed backend for hosts where the OS
//! credential store is unavailable (Windows network logons, OpenSSH
//! sessions): AES-256-GCM over per-account files under
//! `<agent dir>/mcp-oauth-encrypted/<account>/credentials.json`.
//!
//! [VARIANT — design v0.1.5 §5 ruling ①]: the primitives are RustCrypto
//! (`aes-gcm`) and the AAD context string is rpi-branded, so the ciphertext
//! is NOT byte-interoperable with the upstream Node `crypto` envelope
//! (the upstream store is a new surface with no historical compatibility
//! burden; parity is behavioral, not byte-level). The env key name follows
//! the AUTH_SECRET_SERVICE brand-rename precedent:
//! `RPI_MCP_ADAPTER_OAUTH_FILE_KEY` (upstream
//! `PI_MCP_ADAPTER_OAUTH_FILE_KEY`).
//!
//! Security invariants (G4 + upstream security review):
//! - the key is canonical base64 for EXACTLY 32 random bytes, read from the
//!   environment at every operation (rotation takes effect without a
//!   restart);
//! - every envelope field is canonical base64 (re-encode equality check —
//!   non-canonical encodings are rejected, not silently accepted);
//! - the account name is bound into the AEAD additional data
//!   (`context\0account`), so a ciphertext swapped between accounts fails
//!   its tag;
//! - credential files must be private regular files (symlinks refused,
//!   group/other permission bits refused on unix) and live in 0700
//!   directories; writes go through a fresh 0600 temp file + fsync +
//!   atomic rename;
//! - write failures clean the temp file up; token values never reach logs.

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes256Gcm;
use base64::Engine;
use serde_json::Value;

use crate::error::AdapterError;
use crate::oauth::store::SecretStore;

/// `OAUTH_FILE_KEY_ENV` (mcp-auth.ts:43) — [VARIANT] rpi brand rename.
pub const OAUTH_FILE_KEY_ENV: &str = "RPI_MCP_ADAPTER_OAUTH_FILE_KEY";

/// `ENCRYPTED_FILE_AAD_CONTEXT` (mcp-auth.ts:44) — [VARIANT] rpi-branded;
/// ciphertext is deliberately not interoperable with the upstream envelope.
const ENCRYPTED_FILE_AAD_CONTEXT: &str = "rpi-mcp-adapter.oauth.encrypted-file.v1";

/// AES-256-GCM key length.
const KEY_BYTES: usize = 32;
/// Standard GCM nonce length.
const IV_BYTES: usize = 12;
/// Standard GCM tag length.
const TAG_BYTES: usize = 16;

/// `decodeCanonicalBase64` (mcp-auth.ts:330-340): a canonical base64 string
/// (standard alphabet, correct padding) that re-encodes to itself and is
/// exactly `expected_bytes` long.
fn decode_canonical_base64(value: &Value) -> Result<Vec<u8>, AdapterError> {
    let text = value.as_str().ok_or_else(|| {
        AdapterError::InvalidConfigValue("value is not canonical base64".to_string())
    })?;
    decode_canonical_base64_str(text, None)
}

fn decode_canonical_base64_str(
    text: &str,
    expected_bytes: Option<usize>,
) -> Result<Vec<u8>, AdapterError> {
    let invalid = || AdapterError::InvalidConfigValue("value is not canonical base64".to_string());
    if text.is_empty() {
        return Err(invalid());
    }
    // Canonical form: standard alphabet, length a multiple of 4, and padding
    // only as `xx==` / `xxx=`. Byte-level check (a `&str` byte slice never
    // panics on multi-byte UTF-8, unlike `&str[..n]` — a non-ASCII key like
    // `éAAA` must fail closed with InvalidConfigValue, not panic).
    let bytes = text.as_bytes();
    let body_ok = !bytes[..bytes.len().saturating_sub(4)]
        .iter()
        .any(|b| !(b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/'))
        && text.len().is_multiple_of(4);
    if !body_ok {
        return Err(invalid());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| invalid())?;
    let reencoded = base64::engine::general_purpose::STANDARD.encode(&decoded);
    if reencoded != text {
        return Err(invalid());
    }
    if let Some(expected) = expected_bytes {
        if decoded.len() != expected {
            return Err(AdapterError::InvalidConfigValue(format!(
                "value must decode to exactly {expected} bytes"
            )));
        }
    }
    Ok(decoded)
}

/// Store-unavailable error carrying the classification inputs
/// (`OAuthCredentialStoreError`, mcp-auth.ts:135-147): the failing
/// operation and the encrypted-file backend marker.
pub fn credential_store_unavailable(
    operation: &'static str,
    backend_encrypted_file: bool,
    cause: String,
) -> AdapterError {
    AdapterError::OAuthCredentialStoreUnavailable {
        operation,
        backend: if backend_encrypted_file {
            "encrypted-file"
        } else {
            "os-keyring"
        },
        cause,
    }
}

/// `getEncryptedFileKey` (mcp-auth.ts:342-349): the env var must hold
/// canonical base64 for exactly 32 bytes; the error message names the env
/// var so the classifier (`format_oauth_credential_store_unavailable`) can
/// route key-missing to the setup guidance.
fn encrypted_file_key() -> Result<Vec<u8>, AdapterError> {
    let encoded = std::env::var(OAUTH_FILE_KEY_ENV).map_err(|_| {
        credential_store_unavailable(
            "read",
            true,
            format!(
                "{OAUTH_FILE_KEY_ENV} is required for the encrypted OAuth credential file store"
            ),
        )
    })?;
    decode_canonical_base64_str(&encoded, Some(KEY_BYTES)).map_err(|_| {
        credential_store_unavailable(
            "read",
            true,
            format!(
                "{OAUTH_FILE_KEY_ENV} must be canonical base64 for exactly {KEY_BYTES} random bytes"
            ),
        )
    })
}

/// `encryptedEntryPath` (mcp-auth.ts:352-354).
fn encrypted_entry_path(root: &Path, account: &str) -> PathBuf {
    root.join(account).join("credentials.json")
}

/// `validatePrivateRegularFile` (mcp-auth.ts:356-369): false when absent;
/// refuse symlinks/non-regular files and (unix) group/other permission
/// bits. `true` = present and private.
fn validate_private_regular_file(path: &Path) -> Result<bool, AdapterError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(AdapterError::CacheIo(err)),
    };
    let refuse = |reason: String| {
        Err(AdapterError::InvalidConfigValue(format!(
            "Refusing non-regular OAuth credential file at {}: {reason}",
            path.display()
        )))
    };
    if !metadata.is_file() {
        return refuse("not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return refuse("group or other permissions present".into());
        }
    }
    Ok(true)
}

/// `ensurePrivateDirectory` (mcp-auth.ts:371-377): recursive 0700 create +
/// post-create validation (must be a directory, not a symlink).
fn ensure_private_directory(path: &Path) -> Result<(), AdapterError> {
    std::fs::create_dir_all(path).map_err(AdapterError::CacheIo)?;
    let metadata = std::fs::symlink_metadata(path).map_err(AdapterError::CacheIo)?;
    if !metadata.is_dir() {
        return Err(AdapterError::InvalidConfigValue(format!(
            "Refusing non-directory OAuth credential path at {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(AdapterError::CacheIo)?;
    }
    Ok(())
}

fn aad_for(account: &str) -> Vec<u8> {
    let mut aad = ENCRYPTED_FILE_AAD_CONTEXT.as_bytes().to_vec();
    aad.push(0);
    aad.extend_from_slice(account.as_bytes());
    aad
}

/// The encrypted-file backend (`createEncryptedFileAuthSecretStore`,
/// mcp-auth.ts:378-428).
pub struct EncryptedFileSecretStore {
    root: PathBuf,
}

impl EncryptedFileSecretStore {
    /// Root: `getAgentPath('mcp-oauth-encrypted')` — the rpi agent dir
    /// (`RPI_CODING_AGENT_DIR` → `~/.rpi/agent`), NOT the configurable
    /// legacy OAuth import dir.
    pub fn new() -> Self {
        Self {
            root: crate::config::get_agent_dir().join("mcp-oauth-encrypted"),
        }
    }

    /// Test seam: an explicit root (sandboxed HOME).
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Default for EncryptedFileSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for EncryptedFileSecretStore {
    fn kind(&self) -> super::SecretStoreKind {
        super::SecretStoreKind::EncryptedFile
    }

    fn read(&self, account: &str) -> Result<Option<String>, AdapterError> {
        let key = encrypted_file_key()?;
        let path = encrypted_entry_path(&self.root, account);
        if !validate_private_regular_file(&path)? {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path).map_err(AdapterError::CacheIo)?;
        let envelope: Value = serde_json::from_str(&raw).map_err(|_| {
            credential_store_unavailable(
                "read",
                true,
                format!(
                    "Unsupported encrypted OAuth credential envelope at {}",
                    path.display()
                ),
            )
        })?;
        let version_ok = envelope.get("version").and_then(Value::as_i64) == Some(1);
        let algorithm_ok = envelope.get("algorithm").and_then(Value::as_str) == Some("aes-256-gcm");
        if !version_ok || !algorithm_ok {
            return Err(credential_store_unavailable(
                "read",
                true,
                format!(
                    "Unsupported encrypted OAuth credential envelope at {}",
                    path.display()
                ),
            ));
        }
        let iv =
            decode_canonical_base64(envelope.get("iv").unwrap_or(&Value::Null)).map_err(|_| {
                credential_store_unavailable("read", true, "invalid envelope iv".into())
            })?;
        if iv.len() != IV_BYTES {
            return Err(credential_store_unavailable(
                "read",
                true,
                format!("invalid envelope iv: must decode to exactly {IV_BYTES} bytes"),
            ));
        }
        let ciphertext = decode_canonical_base64(
            envelope.get("ciphertext").unwrap_or(&Value::Null),
        )
        .map_err(|_| {
            credential_store_unavailable("read", true, "invalid envelope ciphertext".into())
        })?;
        let tag =
            decode_canonical_base64(envelope.get("tag").unwrap_or(&Value::Null)).map_err(|_| {
                credential_store_unavailable("read", true, "invalid envelope tag".into())
            })?;
        if tag.len() != TAG_BYTES {
            return Err(credential_store_unavailable(
                "read",
                true,
                format!("invalid envelope tag: must decode to exactly {TAG_BYTES} bytes"),
            ));
        }

        let mut sealed = ciphertext;
        sealed.extend_from_slice(&tag);
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| {
            credential_store_unavailable("read", true, "invalid encrypted-file key".into())
        })?;
        let nonce = aes_gcm::Nonce::from_slice(&iv);
        let plaintext = cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &sealed,
                    aad: &aad_for(account),
                },
            )
            .map_err(|_| {
                credential_store_unavailable(
                    "read",
                    true,
                    format!(
                        "Failed to decrypt OAuth credential envelope at {}",
                        path.display()
                    ),
                )
            })?;
        String::from_utf8(plaintext)
            .map_err(|_| {
                credential_store_unavailable("read", true, "decrypted payload is not UTF-8".into())
            })
            .map(Some)
    }

    fn write(&self, account: &str, payload: &str) -> Result<(), AdapterError> {
        let key = encrypted_file_key()?;
        let iv = {
            use aes_gcm::aead::rand_core::RngCore;
            let mut iv = vec![0u8; IV_BYTES];
            aes_gcm::aead::rand_core::OsRng
                .try_fill_bytes(&mut iv)
                .map_err(|e| {
                    credential_store_unavailable(
                        "write",
                        true,
                        format!("OS RNG unavailable for OAuth credential IV: {e}"),
                    )
                })?;
            iv
        };
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| {
            credential_store_unavailable("write", true, "invalid encrypted-file key".into())
        })?;
        let nonce = aes_gcm::Nonce::from_slice(&iv);
        let sealed = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: payload.as_bytes(),
                    aad: &aad_for(account),
                },
            )
            .map_err(|_| {
                credential_store_unavailable(
                    "write",
                    true,
                    "OAuth credential encryption failed".into(),
                )
            })?;
        let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_BYTES);
        let engine = base64::engine::general_purpose::STANDARD;
        let envelope = serde_json::json!({
            "version": 1,
            "algorithm": "aes-256-gcm",
            "iv": engine.encode(&iv),
            "ciphertext": engine.encode(ciphertext),
            "tag": engine.encode(tag),
        })
        .to_string();

        let dir = self.root.join(account);
        ensure_private_directory(&self.root)?;
        ensure_private_directory(&dir)?;
        let destination = encrypted_entry_path(&self.root, account);
        validate_private_regular_file(&destination)?;
        let temporary = dir.join(format!(".credentials-{}.tmp", {
            use aes_gcm::aead::rand_core::RngCore;
            let mut bytes = [0u8; 12];
            aes_gcm::aead::rand_core::OsRng
                .try_fill_bytes(&mut bytes)
                .map_err(|e| {
                    credential_store_unavailable(
                        "write",
                        true,
                        format!("OS RNG unavailable for OAuth credential temp name: {e}"),
                    )
                })?;
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        }));
        let write_result = (|| -> Result<(), AdapterError> {
            use std::io::Write;
            #[cfg(unix)]
            let mut file = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)
                    .map_err(AdapterError::CacheIo)?
            };
            #[cfg(not(unix))]
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(AdapterError::CacheIo)?;
            file.write_all(envelope.as_bytes())
                .map_err(AdapterError::CacheIo)?;
            file.sync_all().map_err(AdapterError::CacheIo)?;
            drop(file);
            std::fs::rename(&temporary, &destination).map_err(AdapterError::CacheIo)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        write_result
    }

    fn remove(&self, account: &str) -> Result<(), AdapterError> {
        // Key presence is validated even for removal (upstream
        // `remove(account) { getEncryptedFileKey(); ... }`).
        encrypted_file_key()?;
        let path = encrypted_entry_path(&self.root, account);
        if !validate_private_regular_file(&path)? {
            return Ok(());
        }
        std::fs::remove_file(&path).map_err(AdapterError::CacheIo)?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(())
    }
}

/// `formatOAuthCredentialStoreUnavailable` (mcp-auth.ts:169-183): map a
/// store-unavailable error to the user-facing setup guidance. [VARIANT]
/// env/setting names are rpi-branded in the message strings.
pub fn format_oauth_credential_store_unavailable(error: &AdapterError) -> Option<String> {
    let AdapterError::OAuthCredentialStoreUnavailable {
        operation: _,
        backend,
        cause,
    } = error
    else {
        return None;
    };
    if *backend == "encrypted-file" {
        if cause.contains(OAUTH_FILE_KEY_ENV) {
            return Some(format!(
                "Encrypted OAuth credential file store unavailable. Set {OAUTH_FILE_KEY_ENV} to canonical base64 for exactly {KEY_BYTES} random bytes and retry."
            ));
        }
        return Some(
            "Encrypted OAuth credential file store unavailable. Check the encrypted credential file and key, then reauthenticate."
                .to_string(),
        );
    }
    if cfg!(target_os = "linux") && {
        let lowered = cause.to_ascii_lowercase();
        lowered.contains("revoked") || lowered.contains("keyrevoked")
    } {
        return Some(
            "OAuth credential store unavailable: the Linux session keyring may be revoked. Start rpi from a fresh login/keyring session and retry."
                .to_string(),
        );
    }
    if cause.contains("ERROR_NO_SUCH_LOGON_SESSION") || regex_1312(cause) {
        return Some(format!(
            "OAuth credential store unavailable: Windows Credential Manager is unavailable from this network logon. To opt in to encrypted file storage for OpenSSH/headless use, set settings.oauthCredentialStore to \"encrypted-file\" and provide {OAUTH_FILE_KEY_ENV}."
        ));
    }
    Some(
        "OAuth credential store unavailable. Configure or unlock the OS credential store and retry."
            .to_string(),
    )
}

/// `\b1312\b` word-boundary match (kept tiny — the full regex crate is not
/// warranted for one literal).
fn regex_1312(cause: &str) -> bool {
    let bytes = cause.as_bytes();
    for (index, window) in bytes.windows(4).enumerate() {
        if window == b"1312"
            && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
            && (index + 4 == bytes.len() || !bytes[index + 4].is_ascii_alphanumeric())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_env<R>(f: impl FnOnce() -> R) -> R {
        // TEST_LOCK is not needed: the env var is process-global but each
        // test sets it before use and the suite is single-threaded per crate
        // default (cargo test threads share process env — serialize via an
        // explicit lock to stay deterministic under --test-threads > 1).
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        f()
    }

    fn valid_key() -> String {
        base64::engine::general_purpose::STANDARD.encode([7u8; KEY_BYTES])
    }

    #[test]
    fn canonical_base64_rejects_noncanonical_and_wrong_length() {
        assert!(decode_canonical_base64_str("AAAA", Some(3)).is_ok());
        assert!(decode_canonical_base64_str("AAA", None).is_err()); // length % 4
        assert!(decode_canonical_base64_str("AA A", None).is_err());
        // Valid base64 of 4 bytes but non-canonical alphabet form is fine;
        // a 4-byte decode asked to be 32 must fail:
        assert!(decode_canonical_base64_str("AAAAAAAA", Some(32)).is_err());
    }

    #[test]
    fn canonical_base64_fails_closed_on_multibyte_utf8() {
        // Regression: the body window used to be sliced as `text[..len-4]`
        // (a `&str` byte range), which PANICKED on multi-byte UTF-8 keys
        // instead of failing closed with InvalidConfigValue.
        for key in ["éAAA", "AéAA", "中中中中", "AAAA😀"] {
            assert!(
                decode_canonical_base64_str(key, None).is_err(),
                "{key:?} must be rejected, not panic"
            );
        }
    }

    #[test]
    fn roundtrip_write_read_remove() {
        key_env(|| {
            std::env::set_var(OAUTH_FILE_KEY_ENV, valid_key());
            let dir = std::env::temp_dir().join(format!(
                "rpi-enc-store-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let store = EncryptedFileSecretStore::with_root(dir.clone());
            store.write("sha256-abc", "{\"tokens\":{}}").expect("write");
            // File exists, is private, and the payload is NOT plaintext.
            let file = encrypted_entry_path(&dir, "sha256-abc");
            let raw = std::fs::read_to_string(&file).expect("envelope");
            assert!(raw.contains("aes-256-gcm"));
            assert!(!raw.contains("tokens"));
            assert_eq!(
                store.read("sha256-abc").expect("read"),
                Some("{\"tokens\":{}}".to_string())
            );
            store.remove("sha256-abc").expect("remove");
            assert_eq!(store.read("sha256-abc").expect("read after remove"), None);
            let _ = std::fs::remove_dir_all(&dir);
            std::env::remove_var(OAUTH_FILE_KEY_ENV);
        })
    }

    #[test]
    fn missing_key_fails_with_env_named_in_cause() {
        key_env(|| {
            std::env::remove_var(OAUTH_FILE_KEY_ENV);
            let dir = std::env::temp_dir().join("rpi-enc-store-missing-key");
            let store = EncryptedFileSecretStore::with_root(dir);
            let err = store.read("sha256-x").expect_err("key required");
            let message = format_oauth_credential_store_unavailable(&err).expect("classified");
            assert!(message.contains(OAUTH_FILE_KEY_ENV));
            assert!(message.starts_with("Encrypted OAuth credential file store unavailable."));
        })
    }

    #[test]
    fn account_swap_fails_tag_verification() {
        key_env(|| {
            std::env::set_var(OAUTH_FILE_KEY_ENV, valid_key());
            let dir = std::env::temp_dir().join(format!(
                "rpi-enc-store-swap-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let store = EncryptedFileSecretStore::with_root(dir.clone());
            store
                .write("sha256-account-a", "secret-payload")
                .expect("write");
            // Move the envelope to another account directory: the AAD bind
            // must make the decryption fail.
            let source = encrypted_entry_path(&dir, "sha256-account-a");
            let target_dir = dir.join("sha256-account-b");
            std::fs::create_dir_all(&target_dir).expect("mkdir");
            std::fs::rename(&source, target_dir.join("credentials.json")).expect("move");
            let err = store.read("sha256-account-b").expect_err("tag mismatch");
            assert!(format_oauth_credential_store_unavailable(&err).is_some());
            let _ = std::fs::remove_dir_all(&dir);
            std::env::remove_var(OAUTH_FILE_KEY_ENV);
        })
    }

    #[test]
    fn symlink_envelope_is_refused() {
        key_env(|| {
            std::env::set_var(OAUTH_FILE_KEY_ENV, valid_key());
            let base = std::env::temp_dir().join(format!(
                "rpi-enc-store-symlink-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let account_dir = base.join("sha256-a");
            std::fs::create_dir_all(&account_dir).expect("mkdir");
            let real_file = base.join("real-credentials.json");
            std::fs::write(&real_file, "{\"version\":1,\"algorithm\":\"aes-256-gcm\"}")
                .expect("write");
            #[cfg(unix)]
            std::os::unix::fs::symlink(&real_file, account_dir.join("credentials.json"))
                .expect("symlink");
            let store = EncryptedFileSecretStore::with_root(base.clone());
            #[cfg(unix)]
            {
                let error = store.read("sha256-a").expect_err("symlink refused");
                assert!(error
                    .to_string()
                    .contains("Refusing non-regular OAuth credential file"));
            }
            let _ = std::fs::remove_dir_all(&base);
            std::env::remove_var(OAUTH_FILE_KEY_ENV);
        })
    }
}
