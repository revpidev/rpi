//! OAuth sign-in and credential storage for remote MCP servers (port of
//! `packages/coding-agent/src/extensions/mcp/oauth.ts` @ abe508e1b).
//!
//! Credentials live in `<agent-dir>/mcp-auth.json`, keyed by server name and
//! URL as `mcp__<name>|<url>` (v1.0.0 `5806068c2`); a legacy URL-only key is
//! taken over atomically by the first server that loads it. Refresh runs
//! under a cross-process file lock, so rotating refresh tokens are never
//! raced (G4: fail-closed; no plaintext fallback).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs2::FileExt;
use rpi_mcp::auth_provider::UnauthorizedContext;
use rpi_mcp::oauth::{
    AuthorizationServerMetadata, ClientMetadataDocumentFn, McpOAuthProvider,
    McpOAuthProviderOptions, McpOAuthState, McpOAuthStateStore, OAuthChallenge,
    OAuthClientInformation, OAuthClientMetadataDocument, OAuthClientProvider, OAuthFlowOptions,
    OAuthFlowResult, OAuthTokens, authorize_mcp, parse_www_authenticate, step_up_scope,
};
use rpi_mcp::{AuthProvider, McpError};
use serde_json::Value;
use sha2::Digest;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::config::McpOAuthConfig;

const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PATH: &str = "/callback";
/// Where rpi serves its Client ID Metadata Documents: `client.json` and
/// `<callback ID>/client.json` (oauth.ts; upstream base is
/// `https://pi.dev/oauth`, mapped to the rpi product domain).
const CLIENT_METADATA_BASE_URL: &str = "https://revpi.dev/oauth";
/// Redirect URI for refreshes when none is stored (`FALLBACK_REDIRECT_URL`).
const FALLBACK_REDIRECT_URL: &str = "http://127.0.0.1/callback";
/// Access tokens this close to expiry are refreshed before they are sent.
const REFRESH_SKEW_MS: f64 = 30_000.0;
/// Bounds each request to the authorization server (`OAUTH_REQUEST_TIMEOUT_MS`,
/// oauth.ts), so an unresponsive one cannot hold the refresh lock, delay
/// shutdown, or keep a sign-in waiting for long.
const OAUTH_REQUEST_TIMEOUT_MS: u64 = 15_000;
/// How long to wait for another process's refresh.
const REFRESH_LOCK_WAIT_MS: u64 = 25_000;
/// Retry spacing while waiting for the refresh lock.
const REFRESH_LOCK_RETRY_MS: u64 = 100;

/// `McpOAuthSettings` (oauth.ts): the config surface the flow uses.
#[derive(Debug, Clone, Default)]
pub struct McpOAuthSettings {
    pub client_id: Option<String>,
    /// Already resolved.
    pub client_secret: Option<String>,
    pub callback_port: Option<u32>,
    /// Loopback redirect URI; see [`McpOAuthConfig::callback_url`].
    pub callback_url: Option<String>,
    pub scope: Option<String>,
    /// `client_name` for dynamic client registration.
    pub client_name: Option<String>,
    /// See `McpOAuthConfig.client_registration`.
    pub client_registration: Option<String>,
    pub auth_server_metadata_url: Option<Url>,
}

impl McpOAuthSettings {
    /// Build from the config entry, resolving no secrets (the caller does
    /// that with the config-value resolver).
    pub fn from_config(oauth: Option<&McpOAuthConfig>) -> Self {
        let Some(oauth) = oauth else {
            return Self::default();
        };
        Self {
            client_id: oauth.client_id.clone(),
            client_secret: oauth.client_secret.clone(),
            callback_port: oauth.callback_port,
            callback_url: oauth.callback_url.clone(),
            scope: oauth.scope.clone(),
            client_name: oauth.client_name.clone(),
            client_registration: oauth.client_registration.clone(),
            auth_server_metadata_url: oauth
                .auth_server_metadata_url
                .as_deref()
                .and_then(|url| Url::parse(url).ok()),
        }
    }
}

/// `CallbackSettings` (oauth.ts).
#[derive(Debug, Clone)]
pub struct CallbackSettings {
    /// Address to listen on.
    pub host: String,
    /// Host name in the redirect URI.
    pub redirect_host: String,
    pub port: Option<u16>,
    pub path: String,
    /// The exact redirect URI, when the port is fixed.
    pub fixed_redirect_url: Option<String>,
}

/// `callbackSettings` (oauth.ts).
pub fn callback_settings(settings: &McpOAuthSettings) -> CallbackSettings {
    let raw = settings
        .callback_url
        .clone()
        .unwrap_or_else(|| format!("http://{CALLBACK_HOST}{CALLBACK_PATH}"));
    let Ok(url) = Url::parse(&raw) else {
        return CallbackSettings {
            host: CALLBACK_HOST.to_owned(),
            redirect_host: CALLBACK_HOST.to_owned(),
            port: settings.callback_port.map(|port| port as u16),
            path: CALLBACK_PATH.to_owned(),
            fixed_redirect_url: None,
        };
    };
    let address = url.host_str().unwrap_or(CALLBACK_HOST).to_owned();
    let port = url
        .port()
        .or_else(|| settings.callback_port.map(|port| port as u16));
    let fixed_redirect_url = if url.port().is_some() {
        settings.callback_url.clone()
    } else if let Some(port) = port {
        let mut with_port = url.clone();
        let _ = with_port.set_port(Some(port));
        Some(with_port.to_string())
    } else {
        None
    };
    CallbackSettings {
        // `localhost` is served on 127.0.0.1; browsers fall back to it.
        host: if address == "localhost" {
            CALLBACK_HOST.to_owned()
        } else {
            address.clone()
        },
        redirect_host: address,
        port,
        path: url.path().to_owned(),
        fixed_redirect_url,
    }
}

/// `mergeScopes` (oauth.ts): both lists, each once.
pub fn merge_scopes(scopes: &[Option<&str>]) -> Option<String> {
    let mut merged: Vec<&str> = Vec::new();
    for scope in scopes.iter().flatten() {
        for part in scope.split_whitespace().filter(|part| !part.is_empty()) {
            if !merged.contains(&part) {
                merged.push(part);
            }
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged.join(" "))
    }
}

/// `storeKeys` (oauth.ts): the v1.0.0 key by name and URL plus the
/// legacy URL-only key.
pub fn store_keys(name: &str, server_url: &str) -> (String, String) {
    let legacy_key = Url::parse(server_url)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| server_url.to_owned());
    (
        format!("{}|{legacy_key}", super::config::mcp_namespace(name)),
        legacy_key,
    )
}

/// `callbackId` (oauth.ts): 12 characters identifying an MCP server URL
/// in callback paths, computed like Codex does (the first 9 bytes of the
/// SHA-256 of the URL without its fragment, base64url).
fn callback_id(server_url: &str) -> String {
    use base64::Engine;
    let normalized = Url::parse(server_url)
        .map(|mut url| {
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|_| server_url.to_owned());
    let digest = sha2::Sha256::digest(normalized.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..9])
}

/// `clientMetadataDocument` (oauth.ts): rpi's Client ID Metadata
/// Document, for `clientRegistration: "cimd"`, chosen like Codex chooses
/// its own. Without the `iss` parameter in authorization responses
/// (RFC 9207), the redirect URI and the document are specific to the MCP
/// server, so a response cannot be mixed up with one from another
/// authorization server (RFC 9700 section 4.4.2.2).
fn client_metadata_document(
    server_url: &str,
    redirect_url: &str,
    metadata: Option<&AuthorizationServerMetadata>,
) -> Result<OAuthClientMetadataDocument, String> {
    let supports_document = metadata
        .and_then(|metadata| metadata.client_id_metadata_document_supported)
        .unwrap_or(false)
        && metadata
            .and_then(|metadata| metadata.token_endpoint_auth_methods_supported.as_ref())
            .is_some_and(|methods| methods.iter().any(|method| method == "none"));
    if !supports_document {
        return Err(
            "The authorization server does not support Client ID Metadata Documents for public clients; remove oauth.clientRegistration \"cimd\""
                .to_owned(),
        );
    }
    if metadata
        .and_then(|metadata| metadata.authorization_response_iss_parameter_supported)
        .unwrap_or(false)
    {
        return Ok(OAuthClientMetadataDocument {
            url: format!("{CLIENT_METADATA_BASE_URL}/client.json"),
            redirect_url: redirect_url.to_owned(),
        });
    }
    let id = callback_id(server_url);
    let mut redirect =
        Url::parse(redirect_url).map_err(|_| "Invalid OAuth redirect URL".to_owned())?;
    redirect.set_path(&format!("{CALLBACK_PATH}/{id}"));
    Ok(OAuthClientMetadataDocument {
        url: format!("{CLIENT_METADATA_BASE_URL}/{id}/client.json"),
        redirect_url: redirect.to_string(),
    })
}

/// Parse the credential store. A missing/empty file is an empty store; a
/// corrupt file is an error, not an empty store: treating a torn file as
/// empty made the next save drop every other server's credentials
/// (v0.1.6 review P2-3; upstream `JSON.parse` throws).
fn parse_states(content: Option<&str>) -> Result<HashMap<String, McpOAuthState>, ()> {
    let Some(content) = content.filter(|content| !content.trim().is_empty()) else {
        return Ok(HashMap::new());
    };
    serde_json::from_str(content).map_err(|_| ())
}

/// Write the store atomically and privately: the temp file is created 0600
/// (no world-readable window; v0.1.6 review P2-3) and renamed over the
/// destination after a successful flush. The temp name carries a random
/// suffix and `create_new`, so a stale temp from a crash can never be
/// truncated into a torn store.
fn write_private_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp-auth.json");
    let mut last_error = None;
    for _ in 0..8 {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temp = parent.join(format!(".{file_name}.{}.{unique}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut write = || -> std::io::Result<()> {
            use std::io::Write;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        };
        if let Err(error) = write() {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        sync_parent_dir(path);
        return Ok(());
    }
    Err(last_error
        .unwrap_or_else(|| std::io::Error::other("could not create a unique mcp-auth temp file")))
}

/// Best-effort parent-directory sync after the atomic rename (the file data
/// was already flushed); unix-only because Windows cannot open a directory
/// as a file. A failure here cannot corrupt the store.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn serialize_states(states: &HashMap<String, McpOAuthState>) -> String {
    let mut text = serde_json::to_string_pretty(&Value::Object(
        states
            .iter()
            .map(|(key, state)| {
                (
                    key.clone(),
                    serde_json::to_value(state).unwrap_or(Value::Null),
                )
            })
            .collect(),
    ))
    .unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

/// Per-server OAuth state (client registration, tokens, pending PKCE
/// verifier) in `mcp-auth.json` (`McpOAuthServerStore`, oauth.ts).
pub struct McpOAuthServerStore {
    path: PathBuf,
    lock_dir: Option<PathBuf>,
    key: String,
    legacy_key: String,
}

/// Why the cross-process refresh lock was not taken. The two cases carry
/// different user-facing diagnoses: a held lock means another process is
/// refreshing; an unavailable lock is a local setup error (v0.1.6 review
/// round 2, O10 — `None` used to collapse both into "another process holds
/// the credential refresh lock").
#[derive(Debug)]
pub enum RefreshLockFailure {
    /// Another process held the lock past the retry budget.
    Held,
    /// The lock directory or file could not be set up (permissions, IO).
    Unavailable(String),
}

/// Whether a `try_lock_exclusive` failure means the lock is held elsewhere.
/// fs2 maps Windows `ERROR_LOCK_VIOLATION` to a raw os error instead of
/// `WouldBlock`, so both shapes are accepted (round-2 review note).
fn is_lock_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // `ERROR_LOCK_VIOLATION`.
        return error.raw_os_error() == Some(33);
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Failure modes of the store lock/read/write path. `save` reports them to
/// the caller; `load` and the credential-store reads keep their fail-soft
/// fallbacks.
#[derive(Debug)]
enum StoreLockFailure {
    Unavailable(String),
    Corrupted,
    Write(String),
}

impl std::fmt::Display for StoreLockFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => {
                write!(formatter, "the credential store is unavailable: {detail}")
            }
            Self::Corrupted => write!(
                formatter,
                "the credential store is corrupted; refusing to overwrite it"
            ),
            Self::Write(detail) => {
                write!(
                    formatter,
                    "the credential store could not be written: {detail}"
                )
            }
        }
    }
}

impl McpOAuthServerStore {
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The file this store writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `load` (oauth.ts): the first server to load legacy state takes it
    /// over inside the storage lock. `None` also covers an unreadable store
    /// (the lock/corruption branches fail closed).
    pub async fn load(&self) -> Option<McpOAuthState> {
        self.with_file_lock(None, |states| {
            if states.contains_key(&self.key) || !states.contains_key(&self.legacy_key) {
                return (states.get(&self.key).cloned(), None);
            }
            let state = states.remove(&self.legacy_key);
            if let Some(state) = &state {
                states.insert(self.key.clone(), state.clone());
            }
            let next = serialize_states(states);
            (state, Some(next))
        })
    }

    /// `save`. A write failure is reported so the sign-in flow can surface
    /// it instead of silently losing the credentials (v0.1.6 review round
    /// 2, O10).
    pub async fn save(&self, state: McpOAuthState) -> Result<(), String> {
        self.with_file_lock_result(|states| {
            states.insert(self.key.clone(), state);
            ((), Some(serialize_states(states)))
        })
        .map_err(|error| error.to_string())
    }

    /// `withRefreshLock` (oauth.ts): a lock file per server, released on
    /// drop, so another process can take over after a crash.
    ///
    /// Lock acquisition failures fail closed: upstream's proper-lockfile
    /// throws and the refresh aborts; proceeding unlocked could race a
    /// rotating refresh token (v0.1.6 review P2-3). The lock directory is
    /// created 0700 like upstream `mkdirSync(..., { mode: 0o700 })`.
    pub async fn with_refresh_lock<T, F, Fut>(&self, action: F) -> Result<T, RefreshLockFailure>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let Some(lock_dir) = &self.lock_dir else {
            return Ok(action().await);
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(lock_dir)
                .map_err(|error| RefreshLockFailure::Unavailable(error.to_string()))?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(lock_dir)
            .map_err(|error| RefreshLockFailure::Unavailable(error.to_string()))?;
        use sha2::Digest;
        let digest = sha2::Sha256::digest(self.key.as_bytes());
        let name = format!("mcp-auth-refresh-{}", &format!("{digest:x}")[..16]);
        let lock_path = lock_dir.join(name);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| RefreshLockFailure::Unavailable(error.to_string()))?;
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(REFRESH_LOCK_WAIT_MS);
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(error) if is_lock_contended(&error) => {
                    if tokio::time::Instant::now() >= deadline {
                        // A lock that is never released must not stall the
                        // flow forever; upstream's retry budget is exhausted
                        // here and the refresh fails closed.
                        return Err(RefreshLockFailure::Held);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(REFRESH_LOCK_RETRY_MS))
                        .await;
                }
                Err(error) => {
                    return Err(RefreshLockFailure::Unavailable(error.to_string()));
                }
            }
        }
        let result = action().await;
        let _ = FileExt::unlock(&lock);
        Ok(result)
    }

    /// Lock/read/action/write with the failures surfaced to the caller.
    fn with_file_lock_result<T, F>(&self, action: F) -> Result<T, StoreLockFailure>
    where
        F: FnOnce(&mut HashMap<String, McpOAuthState>) -> (T, Option<String>),
    {
        let lock_path = self.path.with_extension("json.lock");
        if let Some(parent) = lock_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreLockFailure::Unavailable(error.to_string()))?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| StoreLockFailure::Unavailable(error.to_string()))?;
        lock.lock_exclusive()
            .map_err(|error| StoreLockFailure::Unavailable(error.to_string()))?;
        let content = match std::fs::read_to_string(&self.path) {
            Ok(text) => Some(text),
            // A missing store is an empty store.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            // Any other read failure (permissions, IO) must not read as an
            // empty store: the next save would drop every other server's
            // credentials (round-2 review note).
            Err(error) => {
                let _ = FileExt::unlock(&lock);
                return Err(StoreLockFailure::Unavailable(format!(
                    "the credential store could not be read: {error}"
                )));
            }
        };
        let mut states = match parse_states(content.as_deref()) {
            Ok(states) => states,
            Err(()) => {
                let _ = FileExt::unlock(&lock);
                return Err(StoreLockFailure::Corrupted);
            }
        };
        let (result, next) = action(&mut states);
        if let Some(next) = next
            && let Err(error) = write_private_atomic(&self.path, &next)
        {
            let _ = FileExt::unlock(&lock);
            return Err(StoreLockFailure::Write(error.to_string()));
        }
        let _ = FileExt::unlock(&lock);
        Ok(result)
    }

    /// Fail-soft wrapper for the read paths (`load`, credential lookups).
    fn with_file_lock<T, F>(&self, fallback: T, action: F) -> T
    where
        F: FnOnce(&mut HashMap<String, McpOAuthState>) -> (T, Option<String>),
    {
        match self.with_file_lock_result(action) {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(%error, path = %self.path.display(), "MCP: OAuth store access failed; refusing to touch credentials");
                fallback
            }
        }
    }
}

/// `McpOAuthCredentialStore` (oauth.ts).
pub struct McpOAuthCredentialStore {
    path: PathBuf,
    lock_dir: Option<PathBuf>,
}

impl McpOAuthCredentialStore {
    /// Default: `<agent-dir>/mcp-auth.json` with refresh locks in the agent
    /// directory.
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            path: agent_dir.join("mcp-auth.json"),
            lock_dir: Some(agent_dir.to_path_buf()),
        }
    }

    /// In-memory store (tests): no lock files, refreshes serialized in this
    /// process only.
    pub fn memory(path: PathBuf) -> Self {
        Self {
            path,
            lock_dir: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `forServer` (oauth.ts).
    pub fn for_server(&self, name: &str, server_url: &str) -> McpOAuthServerStore {
        let (key, legacy_key) = store_keys(name, server_url);
        McpOAuthServerStore {
            path: self.path.clone(),
            lock_dir: self.lock_dir.clone(),
            key,
            legacy_key,
        }
    }

    /// `tokens` (oauth.ts): stored tokens, for noticing sign-ins done by
    /// another process. Does not take over legacy state.
    pub fn tokens(&self, name: &str, server_url: &str) -> Option<OAuthTokens> {
        let (key, legacy_key) = store_keys(name, server_url);
        let store = McpOAuthServerStore {
            path: self.path.clone(),
            lock_dir: None,
            key,
            legacy_key,
        };
        store.with_file_lock(None, |states| {
            let tokens = states
                .get(&store.key)
                .or_else(|| states.get(&store.legacy_key))
                .and_then(|state| state.tokens.clone());
            (tokens, None)
        })
    }

    /// `remove` (oauth.ts): whether credentials were stored; removes
    /// legacy state the server would take over.
    pub fn remove(&self, name: &str, server_url: &str) -> bool {
        let (key, legacy_key) = store_keys(name, server_url);
        let store = McpOAuthServerStore {
            path: self.path.clone(),
            lock_dir: None,
            key,
            legacy_key,
        };
        store.with_file_lock(false, |states| {
            let stored = if states.contains_key(&store.key) {
                Some(store.key.clone())
            } else if states.contains_key(&store.legacy_key) {
                Some(store.legacy_key.clone())
            } else {
                None
            };
            let Some(stored) = stored else {
                return (false, None);
            };
            states.remove(&stored);
            (true, Some(serialize_states(states)))
        })
    }
}

/// `registeredRedirectUrls` (oauth.ts).
fn registered_redirect_urls(client: Option<&OAuthClientInformation>) -> Vec<String> {
    client
        .map(|client| client.redirect_uris.clone())
        .unwrap_or_default()
}

/// `createMcpAuthProvider` (oauth.ts): sends the stored access token and
/// refreshes it when it is about to expire or after a 401. Throws
/// [`McpError::AuthorizationRequired`] when the user has to sign in.
pub struct McpAuthProvider {
    server_url: String,
    store: McpOAuthServerStore,
    settings: Arc<dyn Fn() -> McpOAuthSettings + Send + Sync>,
    on_challenge: Arc<dyn Fn(&OAuthChallenge) + Send + Sync>,
    /// Serializes refreshes in this process; the store's file lock covers
    /// other processes.
    refreshing: tokio::sync::Mutex<()>,
}

impl McpAuthProvider {
    pub fn new(
        server_url: String,
        store: McpOAuthServerStore,
        settings: Arc<dyn Fn() -> McpOAuthSettings + Send + Sync>,
        on_challenge: Arc<dyn Fn(&OAuthChallenge) + Send + Sync>,
    ) -> Self {
        Self {
            server_url,
            store,
            settings,
            on_challenge,
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// `createProvider` (oauth.ts): the provider identifies with the
    /// Client ID Metadata Document when `clientRegistration: "cimd"`.
    fn provider_for(
        &self,
        settings: &McpOAuthSettings,
        redirect_url: String,
        on_redirect: Arc<dyn Fn(Url) + Send + Sync>,
    ) -> McpOAuthProvider {
        let client_metadata_document: Option<ClientMetadataDocumentFn> =
            if settings.client_registration.as_deref() == Some("cimd") {
                let server_url = self.server_url.clone();
                let document_redirect = redirect_url.clone();
                Some(Arc::new(move |metadata| {
                    client_metadata_document(&server_url, &document_redirect, metadata)
                        .map(Some)
                        .map_err(rpi_mcp::oauth::OAuthFlowError::Invalid)
                }))
            } else {
                None
            };
        McpOAuthProvider::new(McpOAuthProviderOptions {
            server_url: self.server_url.clone(),
            redirect_url,
            client_metadata: rpi_mcp::oauth::OAuthClientMetadata {
                client_name: Some(
                    settings
                        .client_name
                        .clone()
                        .unwrap_or_else(|| "rpi".to_owned()),
                ),
                ..Default::default()
            },
            client_metadata_document,
            client_id: settings.client_id.clone(),
            client_secret: settings.client_secret.clone(),
            store: Some(Arc::new(StoreAdapter(
                self.store.key.clone(),
                self.store.path.clone(),
            ))),
            on_redirect,
        })
    }

    /// `refresh` (oauth.ts): replace `stale_token`, the access token
    /// that expired or was rejected.
    async fn refresh(
        &self,
        stale_token: Option<String>,
        challenge: Option<OAuthChallenge>,
    ) -> Result<(), McpError> {
        let _in_process = self.refreshing.lock().await;
        let store = &self.store;
        store
            .with_refresh_lock(|| async {
                let state = store.load().await;
                let Some(tokens) = state.as_ref().and_then(|state| state.tokens.clone()) else {
                    return Err(McpError::AuthorizationRequired);
                };
                if Some(tokens.access_token.clone()) != stale_token {
                    return Ok(());
                }
                let Some(_refresh_token) = tokens
                    .refresh_token
                    .clone()
                    .filter(|token| !token.is_empty())
                else {
                    return Err(McpError::AuthorizationRequired);
                };
                let settings = (self.settings)();
                let callback = callback_settings(&settings);
                let redirect_url = callback.fixed_redirect_url.clone().unwrap_or_else(|| {
                    registered_redirect_urls(
                        state
                            .as_ref()
                            .and_then(|state| state.client_information.as_ref()),
                    )
                    .first()
                    .cloned()
                    .unwrap_or_else(|| FALLBACK_REDIRECT_URL.to_owned())
                });
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_millis(OAUTH_REQUEST_TIMEOUT_MS))
                    .build()
                    .ok();
                let provider = self.provider_for(&settings, redirect_url, Arc::new(|_| {}));
                let result = authorize_mcp(
                    &provider,
                    &OAuthFlowOptions {
                        server_url: self.server_url.clone(),
                        resource_metadata_url: challenge
                            .as_ref()
                            .and_then(|challenge| challenge.resource_metadata_url.as_deref())
                            .and_then(|url| Url::parse(url).ok()),
                        authorization_server_metadata_url: settings
                            .auth_server_metadata_url
                            .clone(),
                        scope: challenge
                            .as_ref()
                            .and_then(|challenge| challenge.scope.clone()),
                        client,
                        ..Default::default()
                    },
                )
                .await;
                match result {
                    Ok(OAuthFlowResult::Authorized) => Ok(()),
                    Ok(OAuthFlowResult::Redirect) => Err(McpError::AuthorizationRequired),
                    Err(rpi_mcp::oauth::OAuthFlowError::AuthorizationRequired) => {
                        Err(McpError::AuthorizationRequired)
                    }
                    Err(error) => Err(McpError::Transport(error.to_string())),
                }
            })
            .await
            .map_err(|failure| match failure {
                RefreshLockFailure::Held => McpError::Transport(
                    "another process holds the credential refresh lock".to_owned(),
                ),
                RefreshLockFailure::Unavailable(detail) => McpError::Transport(format!(
                    "the credential refresh lock is unavailable: {detail}"
                )),
            })?
    }
}

/// Adapter making the server store's key the state store key used by
/// [`McpOAuthProvider`] (the provider itself is keyed by URL).
struct StoreAdapter(String, PathBuf);

#[async_trait::async_trait]
impl McpOAuthStateStore for StoreAdapter {
    async fn load(&self) -> Option<McpOAuthState> {
        let store = McpOAuthServerStore {
            path: self.1.clone(),
            lock_dir: None,
            key: self.0.clone(),
            legacy_key: self.0.clone(),
        };
        store.load().await
    }

    async fn save(&self, state: McpOAuthState) {
        let store = McpOAuthServerStore {
            path: self.1.clone(),
            lock_dir: None,
            key: self.0.clone(),
            legacy_key: self.0.clone(),
        };
        if let Err(error) = store.save(state).await {
            // The provider trait cannot propagate a save failure; at least
            // make it visible in the logs instead of losing the credentials
            // silently (v0.1.6 review round 2, O10). The sign-in flow
            // additionally verifies the persisted tokens before reporting
            // success.
            tracing::warn!(%error, path = %self.1.display(), "MCP: OAuth credentials could not be persisted");
        }
    }
}

#[async_trait::async_trait]
impl AuthProvider for McpAuthProvider {
    async fn token(&self) -> Option<String> {
        let state = self.store.load().await;
        let tokens = state.as_ref().and_then(|state| state.tokens.clone())?;
        let expired = state
            .as_ref()
            .and_then(|state| state.tokens_expire_at)
            .is_some_and(|expires_at| expires_at - REFRESH_SKEW_MS <= now_millis());
        if !expired
            || tokens
                .refresh_token
                .as_ref()
                .is_none_or(|token| token.is_empty())
        {
            return Some(tokens.access_token);
        }
        // Failures fall through: the request goes out with the old token and
        // a 401 decides what happens.
        let _ = self.refresh(Some(tokens.access_token), None).await;
        self.store
            .load()
            .await
            .and_then(|state| state.tokens)
            .map(|tokens| tokens.access_token)
    }

    async fn on_unauthorized(&self, context: UnauthorizedContext) -> Result<(), McpError> {
        let challenge = parse_www_authenticate(context.www_authenticate.as_deref());
        (self.on_challenge)(&challenge);
        // A refresh keeps the granted scope, so more scope needs a new
        // sign-in.
        if challenge.error.as_deref() == Some("insufficient_scope") {
            return Err(McpError::AuthorizationRequired);
        }
        self.refresh(context.token, Some(challenge)).await
    }
}

impl McpAuthProvider {
    /// `settled` (oauth.ts): resolves when no refresh is running, so
    /// shutdown does not drop rotated tokens before they are saved.
    pub async fn settled(&self) {
        let _guard = self.refreshing.lock().await;
    }
}

fn now_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or_default()
}

/// `McpSignInPrompt` (oauth.ts).
#[async_trait::async_trait]
pub trait McpSignInPrompt: Send + Sync {
    /// Show the authorization URL and open it in a browser.
    fn show_authorization_url(&self, url: Url);
    /// Ask for the redirect URL from the browser address bar, for when the
    /// browser cannot reach the loopback callback. Resolved `None` means the
    /// user cancelled.
    async fn prompt_for_redirect_url(&self, signal: CancellationToken) -> Option<String>;
}

/// `McpSignInCancelledError` (oauth.ts).
#[derive(Debug, Clone, thiserror::Error)]
#[error("Sign-in cancelled")]
pub struct McpSignInCancelledError;

/// `responseFromRedirectUrl` (oauth.ts): the pasted URL must be the
/// full redirect URI of this sign-in. A server-specific redirect URI tells
/// authorization servers apart, so it must match exactly (oauth.ts).
pub fn response_from_redirect_url(
    input: &str,
    state: &str,
    redirect_url: &Url,
) -> Result<(String, Option<String>), String> {
    let url = Url::parse(input.trim())
        .map_err(|_| "Expected the full redirect URL from the browser address bar".to_owned())?;
    if url.origin() != redirect_url.origin() || url.path() != redirect_url.path() {
        return Err("The redirect URL does not match this sign-in's redirect URI".to_owned());
    }
    let params: HashMap<String, String> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if let Some(error) = params.get("error") {
        return Err(params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone()));
    }
    if params.get("state").map(String::as_str) != Some(state) {
        return Err("The redirect URL belongs to a different sign-in".to_owned());
    }
    let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
        return Err("The redirect URL does not contain an authorization code".to_owned());
    };
    Ok((code.clone(), params.get("iss").cloned()))
}

/// `waitForAuthorizationResponse` (oauth.ts): the browser callback or a
/// pasted redirect URL, whichever comes first. Aborting `signal` aborts the
/// prompt, which then cancels the sign-in.
pub async fn wait_for_authorization_response(
    callback: &rpi_mcp::oauth::OAuthCallbackServer,
    state: &str,
    redirect_url: &Url,
    prompt: &dyn McpSignInPrompt,
    signal: Option<CancellationToken>,
) -> Result<(String, Option<String>), String> {
    let controller = CancellationToken::new();
    let from_browser = callback.wait_for_callback(state, Some(redirect_url.path()));
    let from_user = async {
        let input = prompt.prompt_for_redirect_url(controller.clone()).await;
        let Some(input) = input.filter(|input| !input.trim().is_empty()) else {
            return Err(McpSignInCancelledError.to_string());
        };
        response_from_redirect_url(&input, state, redirect_url)
    };
    tokio::pin!(from_browser);
    tokio::pin!(from_user);
    let result = tokio::select! {
        result = &mut from_browser => result
            .map(|callback| (callback.code, callback.iss))
            .map_err(|error| error.to_string()),
        result = &mut from_user => result,
        () = async {
            match &signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending().await,
            }
        } => Err(McpSignInCancelledError.to_string()),
    };
    controller.cancel();
    result
}

/// `listenForCallback` (oauth.ts): listen on `port`, or on a free port
/// when it is taken and not `required`.
async fn listen_for_callback(
    settings: &CallbackSettings,
    extra_paths: Vec<String>,
    port: Option<u16>,
    required: bool,
) -> Result<rpi_mcp::oauth::OAuthCallbackServer, String> {
    let options = rpi_mcp::oauth::OAuthCallbackServerOptions {
        host: Some(settings.host.clone()),
        redirect_host: Some(settings.redirect_host.clone()),
        path: Some(settings.path.clone()),
        extra_paths,
        render_page: Some(Arc::new(|page| {
            if page.ok {
                rpi_ai::auth::oauth::callback_page::oauth_success_html(
                    "Signed in to the MCP server. You may now close this page.",
                )
            } else {
                rpi_ai::auth::oauth::callback_page::oauth_error_html(
                    &page.message,
                    page.details.as_deref(),
                )
            }
        })),
        ..Default::default()
    };
    let mut first = options.clone();
    first.port = Some(port.unwrap_or(0));
    match rpi_mcp::oauth::OAuthCallbackServer::listen(first).await {
        Ok(server) => Ok(server),
        Err(error) => {
            if required || port.is_none() {
                return Err(error.to_string());
            }
            rpi_mcp::oauth::OAuthCallbackServer::listen(options)
                .await
                .map_err(|error| error.to_string())
        }
    }
}

/// A silent credential write failure must not report a successful sign-in
/// (v0.1.6 review round 2, O10): read the store back and require the
/// tokens to be present before returning success to the user.
async fn verify_sign_in_persisted(store: &McpOAuthServerStore) -> Result<(), String> {
    match store.load().await {
        Some(state) if state.tokens.is_some() => Ok(()),
        Some(_) => Err(
            "signed in, but the OAuth credentials were not persisted; check the MCP auth store write permissions"
                .to_owned(),
        ),
        None => Err(
            "signed in, but the OAuth credential store could not be read back; check the MCP auth store write permissions"
                .to_owned(),
        ),
    }
}

/// `signInMcpServer` (oauth.ts): uses the stored refresh token when
/// possible; otherwise runs the browser authorization code flow.
pub async fn sign_in_mcp_server(options: SignInOptions<'_>) -> Result<(), String> {
    let SignInOptions {
        server_url,
        store,
        settings,
        challenge,
        prompt,
        signal,
    } = options;
    if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
        return Err(McpSignInCancelledError.to_string());
    }
    let stored = store.load().await;
    let step_up = challenge
        .as_ref()
        .and_then(|challenge| challenge.error.as_deref())
        == Some("insufficient_scope");
    let callback_options = callback_settings(&settings);
    // Reuse the port of the registered redirect URI so the registered client
    // stays valid.
    let registered = registered_redirect_urls(
        stored
            .as_ref()
            .and_then(|state| state.client_information.as_ref()),
    );
    let preferred_port = callback_options.port.or_else(|| {
        registered
            .first()
            .and_then(|url| Url::parse(url).ok())
            .and_then(|url| url.port())
    });
    let cimd = settings.client_registration.as_deref() == Some("cimd");
    let callback = listen_for_callback(
        &callback_options,
        // The redirect URI of a server-specific Client ID Metadata Document.
        if cimd {
            vec![format!("{CALLBACK_PATH}/{}", callback_id(&server_url))]
        } else {
            Vec::new()
        },
        preferred_port,
        callback_options.port.is_some(),
    )
    .await?;
    let redirect_url = callback_options
        .fixed_redirect_url
        .clone()
        .unwrap_or_else(|| callback.redirect_url().to_owned());
    let result = async {
        if let Some(stored) = &stored {
            let mut next = stored.clone();
            // Every sign-in gets a fresh `state` parameter.
            next.oauth_state = None;
            // A registered client cannot use another redirect URI, and its
            // tokens belong to it. A Client ID Metadata Document is not
            // stored, so with one, a stored client was registered before
            // and is replaced (oauth.ts).
            let keep_client = settings.client_id.is_some()
                || if cimd {
                    stored.client_information.is_none()
                } else {
                    registered_redirect_urls(stored.client_information.as_ref())
                        .contains(&redirect_url)
                };
            if !keep_client {
                next.client_information = None;
                next.tokens = None;
                next.tokens_expire_at = None;
            }
            store
                .save(next)
                .await
                .map_err(|error| format!("the stored OAuth state could not be saved: {error}"))?;
        }
        let authorization_url: Arc<std::sync::Mutex<Option<Url>>> =
            Arc::new(std::sync::Mutex::new(None));
        let captured = authorization_url.clone();
        let client_metadata_document: Option<ClientMetadataDocumentFn> = if cimd {
            let document_server_url = server_url.clone();
            let document_redirect = redirect_url.clone();
            Some(Arc::new(move |metadata| {
                client_metadata_document(&document_server_url, &document_redirect, metadata)
                    .map(Some)
                    .map_err(rpi_mcp::oauth::OAuthFlowError::Invalid)
            }))
        } else {
            None
        };
        let provider = McpOAuthProvider::new(McpOAuthProviderOptions {
            server_url: server_url.clone(),
            redirect_url: redirect_url.clone(),
            client_metadata: rpi_mcp::oauth::OAuthClientMetadata {
                client_name: Some(
                    settings
                        .client_name
                        .clone()
                        .unwrap_or_else(|| "rpi".to_owned()),
                ),
                ..Default::default()
            },
            client_metadata_document,
            client_id: settings.client_id.clone(),
            client_secret: settings.client_secret.clone(),
            store: Some(Arc::new(StoreAdapter(
                store.key.clone(),
                store.path.clone(),
            ))),
            on_redirect: Arc::new(move |url| {
                *captured.lock().unwrap_or_else(|error| error.into_inner()) = Some(url);
            }),
        });
        let granted_scope = stored
            .as_ref()
            .and_then(|state| state.tokens.as_ref())
            .and_then(|tokens| tokens.scope.as_deref());
        let challenge_scope: Option<String> = if step_up {
            challenge
                .as_ref()
                .and_then(|challenge| step_up_scope(granted_scope, challenge.scope.as_deref()))
        } else {
            challenge
                .as_ref()
                .and_then(|challenge| challenge.scope.clone())
        };
        let flow_scope = merge_scopes(&[settings.scope.as_deref(), challenge_scope.as_deref()]);
        let flow = OAuthFlowOptions {
            server_url: server_url.clone(),
            resource_metadata_url: challenge
                .as_ref()
                .and_then(|challenge| challenge.resource_metadata_url.as_deref())
                .and_then(|url| Url::parse(url).ok()),
            authorization_server_metadata_url: settings.auth_server_metadata_url.clone(),
            scope: flow_scope,
            skip_refresh: step_up,
            // Requests to the authorization server are time-limited
            // (`timedFetch`, oauth.ts).
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(OAUTH_REQUEST_TIMEOUT_MS))
                .build()
                .ok(),
            signal: signal.clone(),
            ..Default::default()
        };
        if authorize_mcp(&provider, &flow)
            .await
            .map_err(|error| error.to_string())?
            == OAuthFlowResult::Authorized
        {
            return verify_sign_in_persisted(store).await;
        }
        let authorization_url = authorization_url
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| "OAuth flow did not produce an authorization URL".to_owned())?;
        let state = provider
            .state()
            .await
            .ok_or_else(|| "OAuth flow did not produce a state parameter".to_owned())?;
        // The flow picks the redirect URI, which may be specific to the MCP
        // server (oauth.ts).
        let authorization_redirect_url = authorization_url
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .map(|(_, value)| value.into_owned())
            .and_then(|value| Url::parse(&value).ok())
            .unwrap_or_else(|| {
                Url::parse(&redirect_url).unwrap_or_else(|_| {
                    // A compile-time constant that always parses.
                    Url::parse(FALLBACK_REDIRECT_URL).expect("valid fallback")
                })
            });
        prompt.show_authorization_url(authorization_url);
        let (code, iss) = wait_for_authorization_response(
            &callback,
            &state,
            &authorization_redirect_url,
            prompt,
            signal.clone(),
        )
        .await?;
        authorize_mcp(
            &provider,
            &OAuthFlowOptions {
                authorization_code: Some(code),
                iss,
                ..flow
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        verify_sign_in_persisted(store).await
    }
    .await;
    callback.close().await;
    // Aborted requests fail with the signal's reason; report them as the
    // cancellation they are (oauth.ts).
    match result {
        Err(_) if signal.as_ref().is_some_and(CancellationToken::is_cancelled) => {
            Err(McpSignInCancelledError.to_string())
        }
        result => result,
    }
}

/// Options for [`sign_in_mcp_server`].
pub struct SignInOptions<'a> {
    pub server_url: String,
    pub store: &'a McpOAuthServerStore,
    pub settings: McpOAuthSettings,
    pub challenge: Option<OAuthChallenge>,
    pub prompt: &'a dyn McpSignInPrompt,
    /// Aborts the sign-in at any step (session shutdown or a cancel key).
    pub signal: Option<CancellationToken>,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn store(name: &str) -> (McpOAuthCredentialStore, TempDir) {
        let dir = TempDir::new();
        let store = McpOAuthCredentialStore::memory(dir.path.join("mcp-auth.json"));
        let _ = name;
        (store, dir)
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rpi-mcp-oauth-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).ok();
        }
    }

    #[tokio::test]
    async fn legacy_url_keys_are_taken_over_atomically() {
        let path =
            std::env::temp_dir().join(format!("rpi-mcp-oauth-legacy-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"https://mcp.example/mcp": {"serverUrl": "https://mcp.example/mcp", "tokens": {"access_token": "old", "token_type": "bearer"}}}"#,
        )
        .unwrap();
        let credentials = McpOAuthCredentialStore::memory(path.clone());
        let server = credentials.for_server("docs", "https://mcp.example/mcp");
        let state = server.load().await.expect("legacy state taken over");
        assert_eq!(state.tokens.unwrap().access_token, "old");
        let (key, legacy) = store_keys("docs", "https://mcp.example/mcp");
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw.get(&key).is_some());
        assert!(raw.get(&legacy).is_none());
        // `tokens` reads either key; `remove` deletes it.
        assert_eq!(
            credentials
                .tokens("docs", "https://mcp.example/mcp")
                .unwrap()
                .access_token,
            "old"
        );
        assert!(credentials.remove("docs", "https://mcp.example/mcp"));
        assert!(
            credentials
                .tokens("docs", "https://mcp.example/mcp")
                .is_none()
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn callback_settings_respect_fixed_ports_and_localhost() {
        let settings = McpOAuthSettings {
            callback_url: Some("http://localhost:8080/oauth/callback".to_owned()),
            ..Default::default()
        };
        let callback = callback_settings(&settings);
        assert_eq!(callback.host, "127.0.0.1");
        assert_eq!(callback.redirect_host, "localhost");
        assert_eq!(callback.port, Some(8080));
        assert_eq!(
            callback.fixed_redirect_url.as_deref(),
            Some("http://localhost:8080/oauth/callback")
        );

        let settings = McpOAuthSettings {
            callback_port: Some(4321),
            ..Default::default()
        };
        let callback = callback_settings(&settings);
        assert_eq!(callback.port, Some(4321));
        assert_eq!(
            callback.fixed_redirect_url.as_deref(),
            Some("http://127.0.0.1:4321/callback")
        );
    }

    #[test]
    fn scopes_merge_without_duplicates() {
        assert_eq!(
            merge_scopes(&[Some("a b"), None, Some("b c")]).as_deref(),
            Some("a b c")
        );
        assert_eq!(merge_scopes(&[None, Some(""), None]), None);
    }

    #[test]
    fn redirect_url_errors_match_upstream() {
        let redirect = Url::parse("http://127.0.0.1:1/callback").unwrap();
        let (code, iss) = response_from_redirect_url(
            "http://127.0.0.1:1/callback?code=x&state=s&iss=https%3A%2F%2Fas.example",
            "s",
            &redirect,
        )
        .unwrap();
        assert_eq!(code, "x");
        assert_eq!(iss.as_deref(), Some("https://as.example"));
        // A server-specific redirect URI must match exactly (oauth.ts).
        assert!(
            response_from_redirect_url("http://127.0.0.1:1/other?code=x&state=s", "s", &redirect)
                .is_err()
        );
        assert!(
            response_from_redirect_url(
                "http://127.0.0.1:2/callback?code=x&state=s",
                "s",
                &redirect
            )
            .is_err()
        );
        assert!(response_from_redirect_url("nonsense", "s", &redirect).is_err());
        assert!(
            response_from_redirect_url(
                "http://127.0.0.1:1/callback?code=x&state=other",
                "s",
                &redirect
            )
            .is_err()
        );
        assert!(
            response_from_redirect_url("http://127.0.0.1:1/callback?state=s", "s", &redirect)
                .is_err()
        );
        assert!(
            response_from_redirect_url(
                "http://127.0.0.1:1/callback?error=denied&state=s",
                "s",
                &redirect
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn credential_store_rewrites_are_atomic_under_lock() {
        let (credentials, dir) = store("docs");
        let server = credentials.for_server("docs", "https://mcp.example/mcp");
        server
            .save(McpOAuthState {
                server_url: "https://mcp.example/mcp".to_owned(),
                client_information: None,
                tokens: Some(OAuthTokens {
                    access_token: "a".to_owned(),
                    token_type: "bearer".to_owned(),
                    ..Default::default()
                }),
                tokens_expire_at: None,
                code_verifier: None,
                oauth_state: None,
                discovery: None,
            })
            .await
            .expect("the credential save must succeed");
        assert_eq!(
            server.load().await.unwrap().tokens.unwrap().access_token,
            "a"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path.join("mcp-auth.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// v0.1.6 review P2-3: a torn store must not read as empty (the next
    /// save would then drop every other server's credentials).
    #[tokio::test]
    async fn corrupt_store_fails_closed_and_is_not_overwritten() {
        let (credentials, dir) = store("docs");
        let path = dir.path.join("mcp-auth.json");
        let torn = "{\"a|url\": {\"serverUrl\":";
        std::fs::write(&path, torn).unwrap();
        let server = credentials.for_server("docs", "https://mcp.example/mcp");
        assert!(
            server.load().await.is_none(),
            "a corrupt store must not read as empty"
        );
        let error = server
            .save(McpOAuthState {
                server_url: "https://mcp.example/mcp".to_owned(),
                client_information: None,
                tokens: Some(OAuthTokens {
                    access_token: "new".to_owned(),
                    token_type: "bearer".to_owned(),
                    ..Default::default()
                }),
                tokens_expire_at: None,
                code_verifier: None,
                oauth_state: None,
                discovery: None,
            })
            .await
            .expect_err("a corrupted store must fail the save closed");
        assert!(error.contains("corrupted"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            torn,
            "the corrupt store must be preserved, not overwritten"
        );
    }

    fn sample_state(access_token: &str) -> McpOAuthState {
        McpOAuthState {
            server_url: "https://mcp.example/mcp".to_owned(),
            client_information: None,
            tokens: Some(OAuthTokens {
                access_token: access_token.to_owned(),
                token_type: "bearer".to_owned(),
                ..Default::default()
            }),
            tokens_expire_at: None,
            code_verifier: None,
            oauth_state: None,
            discovery: None,
        }
    }

    /// V16-08 hardening confirmation (V17-03 FR-E R1): credentials are
    /// bound to the server name AND URL — the v1.0.0 `mcp__<name>|<url>`
    /// key. Same URL under another name or same name at another URL never
    /// shares credentials.
    #[tokio::test]
    async fn credential_keys_bind_name_and_url() {
        let (key, legacy) = store_keys("docs", "https://mcp.example/mcp");
        assert_eq!(key, "mcp__docs|https://mcp.example/mcp");
        assert_eq!(legacy, "https://mcp.example/mcp");

        let dir = TempDir::new();
        let credentials = McpOAuthCredentialStore::memory(dir.path.join("mcp-auth.json"));
        let stored = McpOAuthState {
            server_url: "https://mcp.example/mcp".to_owned(),
            tokens: Some(OAuthTokens {
                access_token: "token-a".to_owned(),
                token_type: "bearer".to_owned(),
                ..Default::default()
            }),
            ..sample_state("unused")
        };
        credentials
            .for_server("docs", "https://mcp.example/mcp")
            .save(stored)
            .await
            .unwrap();
        assert_eq!(
            credentials
                .tokens("docs", "https://mcp.example/mcp")
                .map(|tokens| tokens.access_token),
            Some("token-a".to_owned())
        );
        assert!(
            credentials
                .tokens("other", "https://mcp.example/mcp")
                .is_none(),
            "the same URL under another name must not share credentials"
        );
        assert!(
            credentials
                .tokens("docs", "https://other.example/mcp")
                .is_none(),
            "the same name at another URL must not share credentials"
        );
        // The legacy URL-only key is only a read/take-over source; the
        // v1.0.0 key is what new writes use.
        assert!(legacy != key);
        std::fs::remove_dir_all(&dir.path).ok();
    }

    /// Round-2 O10: a write failure is reported to the caller instead of
    /// being swallowed (the sign-in flow turns it into a user-visible
    /// error). Unix-only: the setup simulates the failure with a read-only
    /// parent directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn save_reports_write_failures() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let path = dir.path.join("mcp-auth.json");
        // A readable store and a pre-created lock file in a read-only
        // directory: opening the existing files succeeds, but the temp file
        // cannot be created, which `save` must report as a write failure
        // (not lock contention).
        std::fs::write(&path, r#"{"k": {"serverUrl": "https://mcp.example/mcp"}}"#).unwrap();
        std::fs::write(dir.path.join("mcp-auth.json.lock"), b"").unwrap();
        let server = McpOAuthServerStore {
            path,
            lock_dir: None,
            key: "k".to_owned(),
            legacy_key: "k".to_owned(),
        };
        #[cfg(unix)]
        {
            std::fs::set_permissions(&dir.path, std::fs::Permissions::from_mode(0o555)).unwrap();
            // Root (or a mode-ignoring filesystem) bypasses the permission
            // check; the write would succeed, so skip the assertion there.
            let probe = dir.path.join(".probe");
            let enforced = std::fs::write(&probe, b"x").is_err();
            let _ = std::fs::remove_file(&probe);
            if !enforced {
                std::fs::set_permissions(&dir.path, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
                return;
            }
        }
        let error = server
            .save(sample_state("a"))
            .await
            .expect_err("a failed write must be reported");
        #[cfg(unix)]
        std::fs::set_permissions(&dir.path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.contains("could not be written"), "{error}");
        assert!(
            !error.contains("another process"),
            "a write failure is not lock contention: {error}"
        );
    }

    /// Round-2 note: an unreadable store must not read as empty (the next
    /// save would drop every other server's credentials).
    #[tokio::test]
    async fn unreadable_store_fails_closed_instead_of_reading_empty() {
        let dir = TempDir::new();
        let path = dir.path.join("mcp-auth.json");
        // A directory cannot be read as a file (EISDIR, not NotFound).
        std::fs::create_dir_all(&path).unwrap();
        let server = McpOAuthServerStore {
            path,
            lock_dir: None,
            key: "k".to_owned(),
            legacy_key: "k".to_owned(),
        };
        assert!(server.load().await.is_none(), "load stays fail-soft");
        let error = server
            .save(sample_state("a"))
            .await
            .expect_err("an unreadable store must fail the save");
        assert!(error.contains("could not be read"), "{error}");
    }

    /// Round-2 note: contention classification accepts `WouldBlock` (the
    /// Unix shape) and, on Windows, fs2's raw `ERROR_LOCK_VIOLATION`.
    #[test]
    fn lock_contention_classification() {
        assert!(is_lock_contended(&std::io::Error::from(
            std::io::ErrorKind::WouldBlock
        )));
        assert!(!is_lock_contended(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    /// Round-2 O10: the refresh lock reports contention and local setup
    /// failures distinctly.
    #[tokio::test(start_paused = true)]
    async fn refresh_lock_reports_contention_distinctly() {
        let dir = TempDir::new();
        let path = dir.path.join("mcp-auth.json");
        let lock_dir = dir.path.join("locks");
        let server = McpOAuthServerStore {
            path: path.clone(),
            lock_dir: Some(lock_dir.clone()),
            key: "docs|https://mcp.example/mcp".to_owned(),
            legacy_key: "https://mcp.example/mcp".to_owned(),
        };
        // Uncontended: the action runs under the lock.
        assert_eq!(
            server.with_refresh_lock(|| async { 7u8 }).await.unwrap(),
            7u8
        );

        // Contended: hold the lock file like a concurrent process. Paused
        // time advances the retry sleeps to the 25s budget instantly.
        use sha2::Digest;
        let digest = sha2::Sha256::digest(server.key.as_bytes());
        let name = format!("mcp-auth-refresh-{}", &format!("{digest:x}")[..16]);
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_dir.join(name))
            .unwrap();
        held.lock_exclusive().unwrap();
        let failure = server
            .with_refresh_lock(|| async { 9u8 })
            .await
            .expect_err("a held lock must fail closed");
        assert!(matches!(failure, RefreshLockFailure::Held), "{failure:?}");
        FileExt::unlock(&held).unwrap();

        // Unavailable: the lock directory path is an existing file.
        std::fs::write(&path, "{}").unwrap();
        let blocked = McpOAuthServerStore {
            path,
            lock_dir: Some(dir.path.join("mcp-auth.json")),
            key: "docs|https://mcp.example/mcp".to_owned(),
            legacy_key: "https://mcp.example/mcp".to_owned(),
        };
        let failure = blocked
            .with_refresh_lock(|| async { 9u8 })
            .await
            .expect_err("an unavailable lock must fail closed");
        assert!(
            matches!(failure, RefreshLockFailure::Unavailable(_)),
            "{failure:?}"
        );
    }

    /// Round-2 O10: the sign-in read-back requires persisted tokens.
    #[tokio::test]
    async fn verify_sign_in_requires_persisted_tokens() {
        let dir = TempDir::new();
        let server = McpOAuthServerStore {
            path: dir.path.join("mcp-auth.json"),
            lock_dir: None,
            key: "k".to_owned(),
            legacy_key: "k".to_owned(),
        };
        let error = verify_sign_in_persisted(&server)
            .await
            .expect_err("no persisted tokens must fail the read-back");
        assert!(error.contains("could not be read back"), "{error}");
        server.save(sample_state("tok")).await.expect("save");
        verify_sign_in_persisted(&server)
            .await
            .expect("persisted tokens verify");
    }

    // -- CIMD (#10302) integration tests ---------------------------------

    /// A local authorization server stub for the sign-in flow: metadata,
    /// `/authorize` (302 to the redirect URI), `/token`, and a counted
    /// `/register`.
    #[derive(Default)]
    struct CimdState {
        registrations: std::sync::atomic::AtomicUsize,
        token_forms: Mutex<Vec<HashMap<String, String>>>,
        issuer: Mutex<String>,
        cimd_supported: std::sync::atomic::AtomicBool,
        iss_parameter: std::sync::atomic::AtomicBool,
    }

    async fn cimd_handle(
        axum::extract::State(state): axum::extract::State<Arc<CimdState>>,
        request: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response {
        let path = request.uri().path().to_owned();
        let query = request.uri().query().unwrap_or_default().to_owned();
        let base = state.issuer.lock().unwrap().clone();
        match path.as_str() {
            "/.well-known/oauth-authorization-server" => {
                let metadata = serde_json::json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                    "registration_endpoint": format!("{base}/register"),
                    "response_types_supported": ["code"],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": ["none"],
                    "client_id_metadata_document_supported": state
                        .cimd_supported
                        .load(std::sync::atomic::Ordering::SeqCst),
                    "authorization_response_iss_parameter_supported": state
                        .iss_parameter
                        .load(std::sync::atomic::Ordering::SeqCst),
                });
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(metadata.to_string()))
                    .unwrap()
            }
            "/authorize" => {
                let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect();
                let redirect = params.get("redirect_uri").cloned().unwrap_or_default();
                let state_param = params.get("state").cloned().unwrap_or_default();
                let mut location = format!("{redirect}?code=cimd-code&state={state_param}");
                if state
                    .iss_parameter
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    location.push_str(&format!("&iss={base}"));
                }
                axum::response::Response::builder()
                    .status(302)
                    .header("location", location)
                    .body(axum::body::Body::empty())
                    .unwrap()
            }
            "/token" => {
                let body = axum::body::to_bytes(request.into_body(), 65536)
                    .await
                    .unwrap_or_default();
                state.token_forms.lock().unwrap().push(
                    url::form_urlencoded::parse(&body)
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect(),
                );
                let tokens = serde_json::json!({
                    "access_token": "access-1",
                    "token_type": "bearer",
                    "expires_in": 3600,
                    "refresh_token": "refresh-1",
                });
                axum::response::Response::builder()
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(tokens.to_string()))
                    .unwrap()
            }
            "/register" => {
                state
                    .registrations
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                axum::response::Response::builder()
                    .status(500)
                    .body(axum::body::Body::from("must not register"))
                    .unwrap()
            }
            _ => axum::response::Response::builder()
                .status(404)
                .body(axum::body::Body::empty())
                .unwrap(),
        }
    }

    async fn start_cimd_server(cimd_supported: bool, iss_parameter: bool) -> (Url, Arc<CimdState>) {
        let state = Arc::new(CimdState::default());
        state
            .cimd_supported
            .store(cimd_supported, std::sync::atomic::Ordering::SeqCst);
        state
            .iss_parameter
            .store(iss_parameter, std::sync::atomic::Ordering::SeqCst);
        let app = axum::Router::new()
            .fallback(cimd_handle)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        *state.issuer.lock().unwrap() = format!("http://{address}");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (Url::parse(&format!("http://{address}")).unwrap(), state)
    }

    /// A browser stand-in: fetching the authorization URL follows its 302
    /// redirect to the loopback callback; pasting a URL never resolves.
    struct BrowserPrompt;

    #[async_trait::async_trait]
    impl McpSignInPrompt for BrowserPrompt {
        fn show_authorization_url(&self, url: Url) {
            tokio::spawn(async move {
                let _ = reqwest::get(url).await;
            });
        }

        async fn prompt_for_redirect_url(&self, signal: CancellationToken) -> Option<String> {
            signal.cancelled().await;
            None
        }
    }

    async fn cimd_settings() -> McpOAuthSettings {
        McpOAuthSettings {
            client_registration: Some("cimd".to_owned()),
            ..Default::default()
        }
    }

    /// #10302: with the `iss` parameter the shared document is used and no
    /// client is registered; without it the document and callback path are
    /// specific to the MCP server.
    #[tokio::test]
    async fn cimd_uses_the_metadata_document_instead_of_registering() {
        for iss_parameter in [true, false] {
            let (as_url, state) = start_cimd_server(true, iss_parameter).await;
            let credentials = McpOAuthCredentialStore::memory(std::env::temp_dir().join(format!(
                "rpi-cimd-{}-{iss_parameter}.json",
                std::process::id()
            )));
            let server_url = format!("{as_url}/mcp");
            let server = credentials.for_server("issues", &server_url);
            sign_in_mcp_server(SignInOptions {
                server_url: server_url.clone(),
                store: &server,
                settings: cimd_settings().await,
                challenge: None,
                prompt: &BrowserPrompt,
                signal: None,
            })
            .await
            .expect("cimd sign-in");
            assert_eq!(
                state
                    .registrations
                    .load(std::sync::atomic::Ordering::SeqCst),
                0,
                "cimd must not register dynamically"
            );
            let (token_count, client_id, redirect_uri) = {
                let forms = state.token_forms.lock().unwrap();
                (
                    forms.len(),
                    forms[0].get("client_id").cloned(),
                    forms[0].get("redirect_uri").cloned(),
                )
            };
            assert_eq!(token_count, 1);
            let expected_client = if iss_parameter {
                format!("{CLIENT_METADATA_BASE_URL}/client.json")
            } else {
                format!(
                    "{CLIENT_METADATA_BASE_URL}/{}/client.json",
                    callback_id(&server_url)
                )
            };
            assert_eq!(client_id.as_deref(), Some(expected_client.as_str()));
            let redirect =
                Url::parse(redirect_uri.as_deref().expect("redirect_uri")).expect("redirect URL");
            if iss_parameter {
                assert_eq!(redirect.path(), "/callback");
            } else {
                assert_eq!(
                    redirect.path(),
                    format!("/callback/{}", callback_id(&server_url))
                );
            }
            // The document is not stored, so a second sign-in refreshes the
            // tokens with the same client id instead of discarding them.
            sign_in_mcp_server(SignInOptions {
                server_url: server_url.clone(),
                store: &server,
                settings: cimd_settings().await,
                challenge: None,
                prompt: &BrowserPrompt,
                signal: None,
            })
            .await
            .expect("cimd refresh");
            let (token_count, grant_type, client_id) = {
                let forms = state.token_forms.lock().unwrap();
                (
                    forms.len(),
                    forms[1].get("grant_type").cloned(),
                    forms[1].get("client_id").cloned(),
                )
            };
            assert_eq!(token_count, 2);
            assert_eq!(grant_type.as_deref(), Some("refresh_token"));
            assert_eq!(client_id.as_deref(), Some(expected_client.as_str()));
        }
    }

    /// #10302: an authorization server without CIMD support reports the
    /// upstream error instead of registering.
    #[tokio::test]
    async fn cimd_requires_authorization_server_support() {
        let (as_url, state) = start_cimd_server(false, false).await;
        let credentials = McpOAuthCredentialStore::memory(
            std::env::temp_dir().join(format!("rpi-cimd-unsupported-{}.json", std::process::id())),
        );
        let server_url = format!("{as_url}/mcp");
        let server = credentials.for_server("issues", &server_url);
        let error = sign_in_mcp_server(SignInOptions {
            server_url,
            store: &server,
            settings: cimd_settings().await,
            challenge: None,
            prompt: &BrowserPrompt,
            signal: None,
        })
        .await
        .expect_err("cimd without support must fail");
        assert!(
            error.contains(
                "The authorization server does not support Client ID Metadata Documents for public clients; remove oauth.clientRegistration \"cimd\""
            ),
            "{error}"
        );
        assert_eq!(
            state
                .registrations
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    /// #10302: a stored dynamically registered client is replaced when the
    /// server switches to the document (its tokens are not refreshed under
    /// another client id).
    #[tokio::test]
    async fn cimd_replaces_a_registered_client() {
        let (as_url, state) = start_cimd_server(true, true).await;
        let credentials = McpOAuthCredentialStore::memory(
            std::env::temp_dir().join(format!("rpi-cimd-replace-{}.json", std::process::id())),
        );
        let server_url = format!("{as_url}/mcp");
        let server = credentials.for_server("issues", &server_url);
        server
            .save(McpOAuthState {
                server_url: server_url.clone(),
                client_information: Some(OAuthClientInformation {
                    client_id: "registered-client".to_owned(),
                    redirect_uris: vec!["http://127.0.0.1:1/callback".to_owned()],
                    ..Default::default()
                }),
                tokens: Some(OAuthTokens {
                    access_token: "stale".to_owned(),
                    token_type: "bearer".to_owned(),
                    refresh_token: Some("old-refresh".to_owned()),
                    ..Default::default()
                }),
                tokens_expire_at: None,
                code_verifier: None,
                oauth_state: None,
                discovery: None,
            })
            .await
            .unwrap();
        sign_in_mcp_server(SignInOptions {
            server_url,
            store: &server,
            settings: cimd_settings().await,
            challenge: None,
            prompt: &BrowserPrompt,
            signal: None,
        })
        .await
        .expect("cimd sign-in replaces the client");
        let (grant_type, client_id) = {
            let forms = state.token_forms.lock().unwrap();
            (
                forms[0].get("grant_type").cloned(),
                forms[0].get("client_id").cloned(),
            )
        };
        assert_eq!(grant_type.as_deref(), Some("authorization_code"));
        let expected_client = format!("{CLIENT_METADATA_BASE_URL}/client.json");
        assert_eq!(client_id.as_deref(), Some(expected_client.as_str()));
        let stored = server.load().await.unwrap();
        assert!(stored.client_information.is_none());
    }

    /// #10565: an already-aborted signal cancels the sign-in before any
    /// request is made.
    #[tokio::test]
    async fn an_aborted_sign_in_returns_cancelled() {
        let credentials = McpOAuthCredentialStore::memory(
            std::env::temp_dir().join(format!("rpi-cimd-cancel-{}.json", std::process::id())),
        );
        let server = credentials.for_server("issues", "https://example.invalid/mcp");
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = sign_in_mcp_server(SignInOptions {
            server_url: "https://example.invalid/mcp".to_owned(),
            store: &server,
            settings: McpOAuthSettings::default(),
            challenge: None,
            prompt: &BrowserPrompt,
            signal: Some(cancelled),
        })
        .await
        .expect_err("cancelled sign-in");
        assert!(error.contains("Sign-in cancelled"), "{error}");
    }
}
