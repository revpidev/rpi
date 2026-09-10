//! Connection table: single-flight connect/reconnect, connection status
//! (`connected` / `closed` / `needs-auth`), close semantics (FR-P0-06/07/08
//! consumer side, design §2.2).
//!
//! Port of `server-manager.ts` (`McpServerManager`) @ pi-mcp-adapter v2.24.0
//! (3d953f90), P0 cut: stdio + HTTP (streamable → SSE fallback) transports,
//! needs-auth marking, single-flight `connect`, `close`/`close_all`, idle
//! accounting. Excluded in P0: rmcp-mux unix sockets (P2), sampling/
//! elicitation handlers (P2), tracing (P2). HTTP connection failures are
//! enriched with a `mcp-probe.ts` classification suffix (TE-D05), and every
//! transport initializes through the `protocolVersion` mode of the server
//! entry (TE-D12).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::{BoxFuture, FutureExt, Shared};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::metadata::ServerEntry;
use crate::oauth::store::{AuthStorageOptions, OAuthCredentialStore};
use crate::protocol::http::{
    resolve_http_config_with_server, HttpConfig, LegacySseTransport, StreamableHttpTransport,
    SSE_FALLBACK_STATUSES,
};
use crate::protocol::stdio::connect_stdio;
use crate::protocol::{DiscoveredMetadata, McpClient, ProtocolError, ProtocolVersionMode};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `ServerConnection.status` (server-manager.ts:132).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Closed,
    NeedsAuth,
}

/// `ServerConnection` (server-manager.ts:120-135). `client` is `None` for
/// `needs-auth` connections (the handshake never completed).
pub struct ServerConnection {
    pub client: Option<Arc<McpClient>>,
    pub definition: ServerEntry,
    /// Authoritative tool catalog; swappable in place by the keep-alive
    /// refresh (`refreshTools` swaps under this lock; readers snapshot).
    pub tools: Mutex<Vec<Value>>,
    pub resources: Vec<Value>,
    pub prompts: Vec<Value>,
    pub prompt_discovery_failed: bool,
    pub instructions: Option<String>,
    pub last_used_at: AtomicU64,
    pub in_flight: AtomicUsize,
    pub status: Mutex<ConnectionStatus>,
    pub credentials_invalidated: AtomicBool,
    /// Cache hints from the server's `tools/list` (server-manager.ts:138 @
    /// 10a45367, #446); written into the metadata cache entry.
    pub tool_list_hints: Option<crate::protocol::ToolListHints>,
}

impl ServerConnection {
    pub fn status(&self) -> ConnectionStatus {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Snapshot of the tool catalog (readers clone; the refresh swaps).
    pub fn tools_snapshot(&self) -> Vec<Value> {
        self.tools.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `tools.len()` without the clone.
    pub fn tools_len(&self) -> usize {
        self.tools.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Swap the catalog after a keep-alive refresh detected a change.
    fn set_tools(&self, tools: Vec<Value>) {
        *self.tools.lock().unwrap_or_else(|e| e.into_inner()) = tools;
    }

    pub fn touch(&self) {
        self.last_used_at.store(now_ms(), Ordering::SeqCst);
    }
}

type ConnectResult = Result<Arc<ServerConnection>, ProtocolError>;
type SharedConnect = Shared<BoxFuture<'static, ConnectResult>>;

/// `supportsOAuth` (mcp-auth-flow.ts:941-955): drives whether a 401 becomes
/// `needs-auth` (P0 stops at the status + guidance text; OAuth is P1).
pub fn supports_oauth(definition: &ServerEntry) -> bool {
    if definition.get("url").is_none_or(Value::is_null) {
        return false;
    }
    if definition.get("auth") == Some(&Value::Bool(false)) {
        return false;
    }
    if definition.get("oauth") == Some(&Value::Bool(false)) {
        return false;
    }
    if definition.get_str("auth") == Some("oauth") {
        return true;
    }
    let has_headers = definition
        .get("headers")
        .and_then(Value::as_object)
        .is_some_and(|h| !h.is_empty());
    if has_headers {
        return false;
    }
    definition.get("auth").is_none()
}

/// `McpServerManager` (server-manager.ts:141-1101), P0 cut.
/// `ToolRefreshResult` (server-manager.ts:172 @ 10a45367).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRefreshResult {
    Updated,
    Unchanged,
    Superseded,
    RefreshTimeout,
}

/// `KEEP_ALIVE_REFRESH_TIMEOUT_MS` (server-manager.ts:170 @ 10a45367): the
/// bounded `tools/list` refresh that doubles as the health probe. A server
/// slower than this stays healthy (#400) — the refresh is only deferred.
pub const KEEP_ALIVE_REFRESH_TIMEOUT_MS: Duration = Duration::from_millis(5_000);

/// `MetadataListChangedListener` (server-manager.ts:156-159 @ 10a45367):
/// fired when a connection's authoritative catalog changes
/// (`keep-alive-refresh`, `listen-recovered`, …).
pub type MetadataListChangedListener = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct McpServerManager {
    connections: Mutex<HashMap<String, Arc<ServerConnection>>>,
    connect_promises: Mutex<HashMap<String, SharedConnect>>,
    default_cwd: Option<String>,
    default_request_timeout: Mutex<Option<Duration>>,
    runtime_cancel: Mutex<CancellationToken>,
    stopped: AtomicBool,
    metadata_list_changed_listener: Mutex<Option<MetadataListChangedListener>>,
    /// Test hook: credential store used by the HTTP connect path
    /// (production builds the OS-keyring store per attempt). Integration
    /// tests inject a `MemorySecretStore`-backed store to assert the
    /// #422/#423 compare-and-delete wiring without touching the keyring.
    auth_store_override: Mutex<Option<Arc<OAuthCredentialStore>>>,
}

impl McpServerManager {
    pub fn new(default_cwd: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            connections: Mutex::new(HashMap::new()),
            connect_promises: Mutex::new(HashMap::new()),
            default_cwd,
            default_request_timeout: Mutex::new(None),
            runtime_cancel: Mutex::new(CancellationToken::new()),
            stopped: AtomicBool::new(false),
            metadata_list_changed_listener: Mutex::new(None),
            auth_store_override: Mutex::new(None),
        })
    }

    /// `setMetadataListChangedListener` (server-manager.ts:231-233 @
    /// 10a45367).
    pub fn set_metadata_list_changed_listener(&self, listener: MetadataListChangedListener) {
        *self
            .metadata_list_changed_listener
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(listener);
    }

    fn fire_metadata_list_changed(&self, name: &str, reason: &str) {
        let listener = self
            .metadata_list_changed_listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(listener) = listener {
            listener(name, reason);
        }
    }

    /// Test hook (see `auth_store_override`).
    pub fn set_auth_store_override(&self, store: Arc<OAuthCredentialStore>) {
        *self
            .auth_store_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(store);
    }

    fn auth_store(&self) -> Arc<OAuthCredentialStore> {
        self.auth_store_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| Arc::new(OAuthCredentialStore::new(AuthStorageOptions::default())))
    }

    pub fn set_runtime_cancel(&self, cancel: CancellationToken) {
        *self
            .runtime_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = cancel;
    }

    /// `setDefaultRequestTimeoutMs` (server-manager.ts:180-182).
    pub fn set_default_request_timeout(&self, timeout: Option<Duration>) {
        *self
            .default_request_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = timeout;
    }

    /// `getResolvedRequestTimeoutMs` (server-manager.ts:201-206): per-server
    /// `requestTimeoutMs` (> 0) beats the global default; the SDK's own 60s
    /// default applies when neither is set.
    pub fn request_timeout(&self, definition: &ServerEntry) -> Duration {
        let per_server = definition
            .get("requestTimeoutMs")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v > 0.0);
        if let Some(ms) = per_server {
            return Duration::from_millis(ms as u64);
        }
        self.default_request_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(crate::protocol::DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn get_connection(&self, name: &str) -> Option<Arc<ServerConnection>> {
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    pub fn get_all_connections(&self) -> Vec<(String, Arc<ServerConnection>)> {
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn is_connecting(&self, name: &str) -> bool {
        self.connect_promises
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    /// `connect` (server-manager.ts:225-270): single-flight per server name,
    /// connected fast-path touches `lastUsedAt`.
    pub async fn connect(
        self: &Arc<Self>,
        name: &str,
        definition: &ServerEntry,
    ) -> Result<Arc<ServerConnection>, ProtocolError> {
        if definition.is_disabled() {
            return Err(ProtocolError::Protocol(format!(
                "MCP server \"{name}\" is disabled"
            )));
        }
        if self.stopped.load(Ordering::SeqCst) {
            return Err(ProtocolError::Protocol(
                "MCP server manager is closed".to_string(),
            ));
        }
        if let Some(existing) = self.get_connection(name) {
            if existing.status() == ConnectionStatus::Connected {
                existing.touch();
                return Ok(existing);
            }
        }

        let shared = {
            let mut promises = self
                .connect_promises
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(shared) = promises.get(name) {
                shared.clone()
            } else {
                let manager = self.clone();
                let name_owned = name.to_string();
                let definition = definition.clone();
                let future = {
                    let name_owned = name_owned.clone();
                    async move { manager.create_connection(&name_owned, &definition).await }
                }
                .map(|r| r.map(Arc::new))
                .boxed()
                .shared();
                promises.insert(name_owned, future.clone());
                future
            }
        };
        let result = shared.await;
        self.connect_promises
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        let connection = result?;

        if self.stopped.load(Ordering::SeqCst) {
            // The manager was closed while connecting: dispose instead of
            // publishing (upstream generation guard, simplified).
            if let Some(client) = &connection.client {
                let _ = client.close().await;
            }
            return Err(ProtocolError::Closed);
        }
        // Reflect transport-level closes in the connection status
        // (server-manager.ts:453-457, identity-guarded by Arc uniqueness).
        if let Some(client) = &connection.client {
            let weak = Arc::downgrade(&connection);
            client.set_on_close(Arc::new(move || {
                if let Some(connection) = weak.upgrade() {
                    *connection.status.lock().unwrap_or_else(|e| e.into_inner()) =
                        ConnectionStatus::Closed;
                }
            }));
        }
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), connection.clone());
        Ok(connection)
    }

    /// `reconnect` (server-manager.ts:281-326): identity-guarded — only tear
    /// down the connection that was proven stale.
    pub async fn reconnect(
        self: &Arc<Self>,
        name: &str,
        definition: &ServerEntry,
        stale: &Arc<ServerConnection>,
    ) -> Result<Arc<ServerConnection>, ProtocolError> {
        let current = self.get_connection(name);
        let is_same = current.as_ref().is_some_and(|c| Arc::ptr_eq(c, stale));
        if !is_same {
            match current {
                Some(c) => return Ok(c),
                None => return self.connect(name, definition).await,
            }
        }
        self.close(name).await;
        self.connect(name, definition).await
    }

    /// `refreshTools` (server-manager.ts:397-486 @ 10a45367): the bounded,
    /// cache-bypassed `tools/list` that doubles as the keep-alive health
    /// probe. Identity-guarded: a late response from a replaced connection
    /// is ignored (`Superseded`). A server WITHOUT the tools capability is
    /// pinged instead; a slow-but-healthy server times out softly
    /// (`RefreshTimeout` — #400: never marked failed, only deferred).
    pub async fn refresh_tools(
        self: &Arc<Self>,
        name: &str,
        expected: &Arc<ServerConnection>,
    ) -> Result<ToolRefreshResult, ProtocolError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Ok(ToolRefreshResult::Superseded);
        }
        let current = self.get_connection(name);
        let is_current = current.as_ref().is_some_and(|c| Arc::ptr_eq(c, expected));
        if !is_current || expected.status() != ConnectionStatus::Connected {
            return Ok(ToolRefreshResult::Superseded);
        }

        // Bounded: min(resolved request timeout, 5s).
        let timeout = self
            .request_timeout(&expected.definition)
            .min(KEEP_ALIVE_REFRESH_TIMEOUT_MS);

        let Some(client) = expected.client.clone() else {
            return Ok(ToolRefreshResult::Superseded);
        };

        // No tools capability → ping proves the session usable
        // (server-manager.ts:421-431).
        let capabilities = client.server_capabilities();
        let has_tools = capabilities
            .as_ref()
            .is_some_and(|c| c.get("tools").is_some());
        if !has_tools {
            match client.call("ping", None, timeout).await {
                Ok(_) => {}
                Err(ProtocolError::Timeout) => return Ok(ToolRefreshResult::RefreshTimeout),
                Err(error) => return Err(error),
            }
            let still_current = self
                .get_connection(name)
                .is_some_and(|c| Arc::ptr_eq(&c, expected));
            return Ok(if still_current {
                ToolRefreshResult::Unchanged
            } else {
                ToolRefreshResult::Superseded
            });
        }

        match client.fetch_all_tools_shared(timeout).await {
            Err(ProtocolError::Timeout) => {
                let still_current = self
                    .get_connection(name)
                    .is_some_and(|c| Arc::ptr_eq(&c, expected));
                Ok(if still_current {
                    ToolRefreshResult::RefreshTimeout
                } else {
                    ToolRefreshResult::Superseded
                })
            }
            Err(error) => Err(error),
            Ok((tools, _hints)) => {
                let still_current = self
                    .get_connection(name)
                    .is_some_and(|c| Arc::ptr_eq(&c, expected));
                if !still_current {
                    return Ok(ToolRefreshResult::Superseded);
                }
                let unchanged = {
                    let current = expected.tools.lock().unwrap_or_else(|e| e.into_inner());
                    *current == tools
                };
                if unchanged {
                    return Ok(ToolRefreshResult::Unchanged);
                }
                // Swap the catalog under the connection's identity and fire
                // the metadata listener (upstream bumps toolsRevision; the
                // Rust connection swaps the Vec under this lock — readers
                // see either the old or the new full list).
                expected.set_tools(tools);
                self.fire_metadata_list_changed(name, "keep-alive-refresh");
                Ok(ToolRefreshResult::Updated)
            }
        }
    }

    /// `createConnection` (server-manager.ts:328-520), P0 cut.
    async fn create_connection(
        &self,
        name: &str,
        definition: &ServerEntry,
    ) -> Result<ServerConnection, ProtocolError> {
        let configured = [
            definition
                .get_str("command")
                .filter(|s| !s.is_empty())
                .map(|_| "command"),
            definition
                .get_str("url")
                .filter(|s| !s.is_empty())
                .map(|_| "url"),
            definition
                .get_str("socket")
                .filter(|s| !s.is_empty())
                .map(|_| "socket"),
        ]
        .into_iter()
        .flatten()
        .count();
        if configured != 1 {
            return Err(ProtocolError::Protocol(format!(
                "Server {name} must configure exactly one of command, url, or socket"
            )));
        }
        let request_timeout = self.request_timeout(definition);

        if definition.get_str("command").is_some() {
            // #442 (server-manager.ts:799-803 @ 10a45367): diagnose a
            // missing / non-directory `cwd` BEFORE the spawn, so the error
            // names the misconfigured path instead of blaming the
            // executable ("failed to spawn …: Not a directory").
            // `statSync(cwd, { throwIfNoEntry: false })` = `fs::metadata`
            // (follows symlinks; ENOENT → fall through).
            let cwd = crate::utils::resolve_config_path(definition.get("cwd"))
                .ok()
                .flatten()
                .or_else(|| self.default_cwd.clone());
            if let Some(cwd) = cwd {
                match std::fs::metadata(&cwd) {
                    Err(_) => {
                        return Err(ProtocolError::Transport(format!(
                            "MCP server \"{name}\" configured cwd does not exist: \"{cwd}\""
                        )));
                    }
                    Ok(stats) if !stats.is_dir() => {
                        return Err(ProtocolError::Transport(format!(
                            "MCP server \"{name}\" configured cwd is not a directory: \"{cwd}\""
                        )));
                    }
                    Ok(_) => {}
                }
            }
            // V13-07 S1: `!command` secret resolution runs on the blocking pool.
            let (transport, incoming) =
                connect_stdio(definition, self.default_cwd.as_deref()).await?;
            let stderr_tail = transport.child().stderr_tail.clone();
            let client = McpClient::new(Arc::new(transport), incoming);
            let mode =
                crate::protocol::parse_protocol_version_mode(definition.get("protocolVersion"));
            match client
                .initialize_with_version(name, request_timeout, mode)
                .await
            {
                Ok(metadata) => Ok(build_connection(client, definition, metadata)),
                Err(error) => {
                    let _ = client.close().await;
                    // server-manager.ts:509-517: append the captured stderr
                    // tail to the connection failure.
                    let detail = stderr_tail
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .diagnostic();
                    Err(match detail {
                        Some(detail) => ProtocolError::Transport(format!("{error} ({detail})")),
                        None => error,
                    })
                }
            }
        } else if definition.get("url").is_some() {
            self.connect_http(name, definition, request_timeout).await
        } else {
            Err(ProtocolError::Protocol(
                "unix socket transport (rmcp-mux) is P2 scope".to_string(),
            ))
        }
    }

    /// `connectHttpClient` (server-manager.ts:707-847): streamable
    /// first, legacy SSE fallback on the 404/405/406/415 matrix, 401 →
    /// needs-auth when `supportsOAuth`. Non-handshake failures are
    /// enriched with a probe suffix (TE-D05, server-manager.ts:522-530).
    async fn connect_http(
        &self,
        name: &str,
        definition: &ServerEntry,
        request_timeout: Duration,
    ) -> Result<ServerConnection, ProtocolError> {
        let store = self.auth_store();
        let (config, injected_token) = http_config_with_auth(&store, name, definition).await?;
        let supports_oauth = supports_oauth(definition);
        let mode = crate::protocol::parse_protocol_version_mode(definition.get("protocolVersion"));

        match self
            .try_streamable(name, definition, config, request_timeout)
            .await
        {
            Ok(connection) => Ok(connection),
            Err(error @ ProtocolError::Unauthorized) => {
                if supports_oauth {
                    // #422/#423: compare-and-delete the credential that just
                    // failed — never a token another process wrote in the
                    // meantime.
                    invalidate_stored_oauth_token(
                        &store,
                        name,
                        definition,
                        injected_token.as_deref(),
                    );
                    Ok(needs_auth_connection(definition))
                } else {
                    Err(error)
                }
            }
            Err(error @ ProtocolError::Http { status, .. })
                if SSE_FALLBACK_STATUSES.contains(&status) =>
            {
                // `shouldFallbackToSse` (server-manager.ts:74-77): a pinned
                // 2026-07-28 entry never falls back — the legacy SSE
                // transport speaks a pre-2026 protocol, so "upgrading" a
                // pin would silently downgrade the negotiated version.
                if mode == ProtocolVersionMode::Pinned2026 {
                    return Err(enrich_http_connection_error(definition, error).await);
                }
                let (config, sse_token) = http_config_with_auth(&store, name, definition).await?;
                match self
                    .try_sse(name, definition, config, request_timeout)
                    .await
                {
                    Ok(connection) => Ok(connection),
                    Err(error @ ProtocolError::Unauthorized) => {
                        if supports_oauth {
                            invalidate_stored_oauth_token(
                                &store,
                                name,
                                definition,
                                sse_token.as_deref(),
                            );
                            Ok(needs_auth_connection(definition))
                        } else {
                            Err(error)
                        }
                    }
                    // TE-D05: enrich the final SSE failure with a probe.
                    Err(error) => Err(enrich_http_connection_error(definition, error).await),
                }
            }
            // TE-D05: enrich non-handshake streamable failures with a probe.
            Err(error) => Err(enrich_http_connection_error(definition, error).await),
        }
    }

    async fn try_streamable(
        &self,
        name: &str,
        definition: &ServerEntry,
        config: HttpConfig,
        request_timeout: Duration,
    ) -> Result<ServerConnection, ProtocolError> {
        let (transport, incoming) = StreamableHttpTransport::new(config);
        let client = McpClient::new(transport, incoming);
        let mode = crate::protocol::parse_protocol_version_mode(definition.get("protocolVersion"));
        match client
            .initialize_with_version(name, request_timeout, mode)
            .await
        {
            Ok(metadata) => Ok(build_connection(client, definition, metadata)),
            Err(error) => {
                let _ = client.close().await;
                Err(error)
            }
        }
    }

    async fn try_sse(
        &self,
        name: &str,
        definition: &ServerEntry,
        config: HttpConfig,
        request_timeout: Duration,
    ) -> Result<ServerConnection, ProtocolError> {
        let (transport, incoming) = LegacySseTransport::connect(config).await?;
        let client = McpClient::new(transport, incoming);
        let mode = crate::protocol::parse_protocol_version_mode(definition.get("protocolVersion"));
        match client
            .initialize_with_version(name, request_timeout, mode)
            .await
        {
            Ok(metadata) => Ok(build_connection(client, definition, metadata)),
            Err(error) => {
                let _ = client.close().await;
                Err(error)
            }
        }
    }

    /// `close` (server-manager.ts:974-1006): mark closed, remove from the
    /// table first so a late close can never clobber a fresh connection.
    pub async fn close(&self, name: &str) {
        let connection = self
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if let Some(connection) = connection {
            *connection.status.lock().unwrap_or_else(|e| e.into_inner()) = ConnectionStatus::Closed;
            if let Some(client) = &connection.client {
                let _ = client.close().await;
            }
        }
    }

    /// `closeAll` (server-manager.ts:1018-1044): every connection closed;
    /// the G4 no-leftover-process red line rides on
    /// `protocol::stdio::StdioChild::shutdown`.
    pub async fn close_all(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let connections: Vec<Arc<ServerConnection>> = self
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, c)| c)
            .collect();
        for connection in connections {
            *connection.status.lock().unwrap_or_else(|e| e.into_inner()) = ConnectionStatus::Closed;
            if let Some(client) = &connection.client {
                let _ = client.close().await;
            }
        }
    }

    pub fn touch(&self, name: &str) {
        if let Some(connection) = self.get_connection(name) {
            connection.touch();
        }
    }

    pub fn increment_in_flight(&self, name: &str) {
        if let Some(connection) = self.get_connection(name) {
            connection.in_flight.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub fn decrement_in_flight(&self, name: &str) {
        if let Some(connection) = self.get_connection(name) {
            connection.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// `isIdle` (server-manager.ts:1095-1100).
    pub fn is_idle(&self, name: &str, timeout: Duration) -> bool {
        let Some(connection) = self.get_connection(name) else {
            return false;
        };
        if connection.status() != ConnectionStatus::Connected {
            return false;
        }
        if connection.in_flight.load(Ordering::SeqCst) > 0 {
            return false;
        }
        let last_used = connection.last_used_at.load(Ordering::SeqCst);
        now_ms().saturating_sub(last_used) > timeout.as_millis() as u64
    }
}

/// Resolve the HTTP config for `definition` with OAuth credential
/// injection (FR-P1-04): servers that `supports_oauth` (no static
/// `auth: "bearer"`, no configured headers) get `Authorization: Bearer`
/// from the stored OAuth tokens — refreshed first when expired. Upstream
/// parity: the SDK `auth()` provider on the connect path, so a stored
/// token rides the handshake instead of failing a 401 round-trip. The
/// token value never reaches logs (G4 red line).
async fn http_config_with_auth(
    store: &OAuthCredentialStore,
    name: &str,
    definition: &ServerEntry,
) -> Result<(HttpConfig, Option<String>), ProtocolError> {
    let mut config = resolve_http_config_with_server(definition, name)?;
    let mut injected = None;
    if supports_oauth(definition) {
        injected = inject_oauth_authorization(store, name, definition, &mut config).await;
    }
    Ok((config, injected))
}

/// Inject `Authorization: Bearer <token>` into `config.headers` when the
/// store holds a usable access token for this server+URL. Store failures
/// degrade to an unauthenticated connect — the 401 → needs-auth flow
/// surfaces the auth requirement to the caller. Returns the injected token
/// (compare-and-delete identity for #422/#423); the value never reaches
/// logs (G4 red line).
async fn inject_oauth_authorization(
    store: &OAuthCredentialStore,
    name: &str,
    definition: &ServerEntry,
    config: &mut HttpConfig,
) -> Option<String> {
    if config
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    {
        return None; // an explicit Authorization header always wins
    }
    let server_url = crate::utils::resolve_server_url(definition.get("url"))
        .ok()
        .flatten()?;
    match crate::oauth::resolve_access_token(store, name, &server_url, definition).await {
        Ok(Some(token)) => {
            config
                .headers
                .push(("Authorization".to_string(), format!("Bearer {token}")));
            Some(token)
        }
        Ok(None) => None,
        Err(error) => {
            tracing::debug!(server = name, %error, "OAuth credential resolution failed");
            None
        }
    }
}

fn build_connection(
    client: Arc<McpClient>,
    definition: &ServerEntry,
    metadata: DiscoveredMetadata,
) -> ServerConnection {
    ServerConnection {
        instructions: client.instructions(),
        client: Some(client),
        definition: definition.clone(),
        tools: Mutex::new(metadata.tools),
        resources: metadata.resources,
        prompts: metadata.prompts,
        prompt_discovery_failed: metadata.prompt_discovery_failed,
        last_used_at: AtomicU64::new(now_ms()),
        in_flight: AtomicUsize::new(0),
        status: Mutex::new(ConnectionStatus::Connected),
        credentials_invalidated: AtomicBool::new(false),
        tool_list_hints: metadata.tool_list_hints,
    }
}

/// server-manager.ts:495-507 — the needs-auth placeholder connection.
fn needs_auth_connection(definition: &ServerEntry) -> ServerConnection {
    ServerConnection {
        client: None,
        definition: definition.clone(),
        tools: Mutex::new(Vec::new()),
        resources: Vec::new(),
        prompts: Vec::new(),
        prompt_discovery_failed: false,
        instructions: None,
        last_used_at: AtomicU64::new(now_ms()),
        in_flight: AtomicUsize::new(0),
        status: Mutex::new(ConnectionStatus::NeedsAuth),
        credentials_invalidated: AtomicBool::new(true),
        tool_list_hints: None,
    }
}

/// #422/#423 compare-and-delete: read the stored entry and delete it only
/// when its access token equals the credential that just failed. A token
/// another process wrote after our request is preserved. Token values never
/// reach logs (G4 red line).
fn invalidate_stored_oauth_token(
    store: &OAuthCredentialStore,
    server_name: &str,
    definition: &ServerEntry,
    invalidated_token: Option<&str>,
) {
    let Some(invalidated_token) = invalidated_token else {
        return;
    };
    let server_url = match crate::utils::resolve_server_url(definition.get("url")) {
        Ok(Some(url)) => url,
        _ => return,
    };
    match crate::oauth::remove_auth_if_token_matches(
        store,
        server_name,
        &server_url,
        invalidated_token,
    ) {
        Ok(_) => {}
        Err(error) => {
            tracing::debug!(server = server_name, %error, "OAuth credential invalidation failed");
        }
    }
}

/// `probeMcpEndpoint` (mcp-probe.ts:172-186 @ 10a45367): one
/// unauthenticated metadata-only request to classify an HTTP endpoint's
/// protocol shape. Returns a human-readable classification string.
///
/// Port of the three-stage probe strategy: modern (`server/discover` +
/// `2026-07-28`) → legacy-post (`initialize`) → legacy-sse (GET stream).
/// TE-D05: enriches HTTP connection failure error messages.
///
/// #415 (@ 10a45367): ambiguous statuses (202/401/503) report "endpoint
/// shape could not be determined" instead of "does not appear to speak
/// MCP", and the modern stage only falls back to the legacy strategies on
/// `unsupported-modern` or the 400/401/404/405/406/415 fallback matrix.
async fn probe_mcp_endpoint(url: &str) -> Option<String> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(_) => return None,
    };

    // Stage 1: modern probe (server/discover + 2026-07-28)
    let modern_response = client
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "server/discover")
        .body(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "server/discover",
                "params": {}
            })
            .to_string(),
        )
        .send()
        .await
        .ok()?;
    let modern = probe_stage(modern_response, true, true).await;
    if let ProbeOutcome::Mcp { classification } = &modern.outcome {
        return Some(classification.clone());
    }
    let mut ambiguous = ambiguous_not_mcp(&modern.response_status, &modern.content_type);

    // `unsupported-modern` (an ok JSON-RPC envelope that is an error or
    // carries a non-2026-07-28 protocolVersion) OR a fallback-matrix status
    // continues to the legacy strategies; anything else stops here.
    if !matches!(modern.outcome, ProbeOutcome::UnsupportedModern)
        && !MODERN_FALLBACK_STATUSES.contains(&modern.response_status)
    {
        return ambiguous.or_else(|| Some(not_mcp(&modern.response_status, &modern.content_type)));
    }

    // Stage 2: legacy POST (initialize)
    let legacy_response = client
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "rpi-mcp-probe", "version": "2.1.2" }
                }
            })
            .to_string(),
        )
        .send()
        .await
        .ok()?;
    let legacy_post = probe_stage(legacy_response, true, false).await;
    if let ProbeOutcome::Mcp { classification } = &legacy_post.outcome {
        return Some(classification.clone());
    }
    ambiguous = ambiguous
        .or_else(|| ambiguous_not_mcp(&legacy_post.response_status, &legacy_post.content_type));
    if !POST_ENDPOINT_MISMATCH_STATUSES.contains(&legacy_post.response_status) {
        return ambiguous_not_mcp(&legacy_post.response_status, &legacy_post.content_type)
            .or(ambiguous)
            .or_else(|| {
                Some(not_mcp(
                    &legacy_post.response_status,
                    &legacy_post.content_type,
                ))
            });
    }

    // Stage 3: legacy SSE (GET stream)
    let sse_response = client
        .get(url)
        .header("accept", "text/event-stream")
        .send()
        .await
        .ok()?;
    let sse = probe_stage(sse_response, false, false).await;
    match sse.outcome {
        ProbeOutcome::Mcp { classification } => Some(classification),
        _ => ambiguous_not_mcp(&sse.response_status, &sse.content_type)
            .or(ambiguous)
            .or_else(|| Some(not_mcp(&sse.response_status, &sse.content_type))),
    }
}

/// `PROBE_TIMEOUT_MS` (mcp-probe.ts:1 @ 10a45367).
const PROBE_TIMEOUT_SECS: u64 = 5;
/// `MODERN_FALLBACK_STATUSES` (mcp-probe.ts:7 @ 10a45367).
const MODERN_FALLBACK_STATUSES: [u16; 6] = [400, 401, 404, 405, 406, 415];
/// `POST_ENDPOINT_MISMATCH_STATUSES` (mcp-probe.ts:8 @ 10a45367).
const POST_ENDPOINT_MISMATCH_STATUSES: [u16; 4] = [404, 405, 406, 415];
/// `AMBIGUOUS_STATUSES` (mcp-probe.ts:9 @ 10a45367) — #415: 202/401/503 get
/// the "endpoint shape could not be determined" flavors.
const AMBIGUOUS_STATUSES: [u16; 3] = [202, 401, 503];

/// One probe stage's consumed response summary + outcome
/// (`classifyResponse` returns the outcome; `notMcp`/`ambiguousNotMcp`
/// consume the response afterwards — upstream keeps the `Response` object,
/// the Rust port snapshots the fields those helpers read).
struct ProbeStage {
    response_status: u16,
    content_type: String,
    outcome: ProbeOutcome,
}

enum ProbeOutcome {
    Mcp { classification: String },
    UnsupportedModern,
    Unrecognized,
}

/// `notMcp` (mcp-probe.ts:143-156 @ 10a45367): the non-MCP classification
/// with the #415 status flavors.
fn not_mcp(status: &u16, content_type: &str) -> String {
    let description = format!(
        "endpoint returned {} ({})",
        probe_response_kind(content_type),
        status
    );
    let suffix = match *status {
        503 => " — server is temporarily unavailable; MCP endpoint shape could not be determined",
        202 => " — MCP endpoint shape could not be determined",
        401 => " — authentication may be required; MCP endpoint shape could not be determined",
        _ => " — this URL does not appear to speak MCP",
    };
    format!("{description}{suffix}")
}

/// `ambiguousNotMcp` (mcp-probe.ts:158-160 @ 10a45367): the not-MCP
/// classification only for the ambiguous statuses.
fn ambiguous_not_mcp(status: &u16, content_type: &str) -> Option<String> {
    AMBIGUOUS_STATUSES
        .contains(status)
        .then(|| not_mcp(status, content_type))
}

/// Run one probe request + `classifyResponse` (mcp-probe.ts:120-155 @
/// 10a45367). `is_modern` marks the `server/discover` strategy (its
/// classifications and the unsupported-modern check differ); the SSE
/// strategy sets `allow_json = false` but still inspects 401 bodies for
/// the Bearer-challenge classification.
async fn probe_stage(response: reqwest::Response, allow_json: bool, is_modern: bool) -> ProbeStage {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    // Read before the body consume (`Response::text` takes `self`).
    let bearer_challenge = is_bearer_challenge(&response);
    let is_success = response.status().is_success();

    // `getJsonRpcEnvelopeInfo` — parsed only when the strategy allows JSON
    // or the status is 401 (the Bearer-challenge classification reads the
    // body). `None` = not a JSON-RPC 2.0 envelope.
    let envelope: Option<Result<Option<&'static str>, ()>> = if allow_json || status == 401 {
        response
            .text()
            .await
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| {
                if value.get("jsonrpc") != Some(&json!("2.0")) {
                    return None;
                }
                if let Some(result) = value.get("result") {
                    let is_modern_version = result.get("protocolVersion").and_then(Value::as_str)
                        == Some(crate::oauth::MODERN_PROTOCOL_VERSION);
                    return Some(Ok(if is_modern_version {
                        Some(crate::oauth::MODERN_PROTOCOL_VERSION)
                    } else {
                        // A non-string or mismatched protocolVersion: keep a
                        // marker that is != MODERN so the modern check
                        // rejects it (upstream compares `!== MODERN`).
                        Some("")
                    }));
                }
                if value.get("error").is_some() {
                    return Some(Err(()));
                }
                None
            })
    } else {
        None
    };

    let outcome = if is_success && content_type.starts_with("text/event-stream") {
        ProbeOutcome::Mcp {
            classification: "endpoint responded with an MCP event stream".to_string(),
        }
    } else if is_success && allow_json && envelope.is_some() {
        match (&envelope, is_modern) {
            // `strategy.kind === "modern" && (envelope.kind === "error" ||
            // envelope.protocolVersion !== MODERN_PROTOCOL_VERSION)`.
            (Some(Err(())), true) | (Some(Ok(Some(""))), true) => ProbeOutcome::UnsupportedModern,
            _ => ProbeOutcome::Mcp {
                classification: if is_modern {
                    "endpoint supports stateless MCP 2026-07-28 server/discover".to_string()
                } else {
                    "endpoint responded with a JSON-RPC 2.0 envelope".to_string()
                },
            },
        }
    } else if status == 401 && bearer_challenge && envelope.is_some() {
        ProbeOutcome::Mcp {
            classification: if is_modern {
                "endpoint requires Bearer authentication during MCP 2026-07-28 server/discover probing".to_string()
            } else {
                "endpoint requires Bearer authentication and responded with a JSON-RPC 2.0 error"
                    .to_string()
            },
        }
    } else {
        ProbeOutcome::Unrecognized
    };
    ProbeStage {
        response_status: status,
        content_type,
        outcome,
    }
}

/// `responseKind` (mcp-probe.ts:105-110): "HTML" for text/html, the bare
/// content type when present, "an untyped response" when absent.
fn probe_response_kind(content_type: &str) -> String {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if base == "text/html" {
        return "HTML".to_string();
    }
    if base.is_empty() {
        return "an untyped response".to_string();
    }
    base
}

/// `isBearerChallenge` (mcp-probe.ts:113-115): WWW-Authenticate carries a
/// Bearer challenge.
fn is_bearer_challenge(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().to_lowercase().starts_with("bearer"))
        })
}

/// `isTransientHttpConnectError` (server-manager.ts:181-189 @ 10a45367,
/// #411/#424): a 503 anywhere in the error chain is an availability
/// blip. The Rust port surfaces transport HTTP statuses as
/// `ProtocolError::Http { status }` (no wrapped cause chain), so the
/// direct match is the whole chain walk.
fn is_transient_http_connect_error(error: &ProtocolError) -> bool {
    matches!(error, ProtocolError::Http { status: 503, .. })
}

/// `enrichHttpConnectionError` (server-manager.ts:978-988 @ 10a45367):
/// append a probe-based classification suffix to the HTTP connection
/// failure message — except for a transient 503, which gets the dedicated
/// availability suffix WITHOUT running the probe (no amplified requests,
/// no "not MCP" misdiagnosis). The probe itself must NOT carry credentials
/// — it is a metadata-only unauthenticated request (G4 red line).
async fn enrich_http_connection_error(
    definition: &ServerEntry,
    error: ProtocolError,
) -> ProtocolError {
    let original_message = error.to_string();
    if is_transient_http_connect_error(&error) {
        return ProtocolError::Transport(format!(
            "{original_message} — endpoint is temporarily unavailable (HTTP 503)"
        ));
    }
    let url = match crate::utils::resolve_server_url(definition.get("url")) {
        Ok(Some(url)) => url,
        _ => return error,
    };
    match probe_mcp_endpoint(&url).await {
        Some(classification) => {
            ProtocolError::Transport(format!("{original_message} — probe: {classification}"))
        }
        None => error,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry(value: Value) -> ServerEntry {
        ServerEntry(value.as_object().cloned().unwrap_or_default())
    }

    #[test]
    fn supports_oauth_truth_table() {
        // mcp-auth-flow.ts:941-955
        assert!(!supports_oauth(&entry(json!({ "command": "x" }))));
        assert!(supports_oauth(&entry(
            json!({ "url": "https://a.test/mcp" })
        )));
        assert!(supports_oauth(&entry(
            json!({ "url": "https://a.test/mcp", "auth": "oauth" })
        )));
        assert!(!supports_oauth(&entry(
            json!({ "url": "https://a.test/mcp", "auth": false })
        )));
        assert!(!supports_oauth(&entry(
            json!({ "url": "https://a.test/mcp", "oauth": false })
        )));
        assert!(!supports_oauth(&entry(json!({
            "url": "https://a.test/mcp",
            "headers": { "X-Key": "v" }
        }))));
        assert!(!supports_oauth(&entry(
            json!({ "url": "https://a.test/mcp", "auth": "bearer" })
        )));
    }

    #[test]
    fn per_server_timeout_beats_default() {
        let manager = McpServerManager::new(None);
        manager.set_default_request_timeout(Some(Duration::from_millis(1500)));
        assert_eq!(
            manager.request_timeout(&entry(json!({ "requestTimeoutMs": 250 }))),
            Duration::from_millis(250)
        );
        assert_eq!(
            manager.request_timeout(&entry(json!({}))),
            Duration::from_millis(1500)
        );
        assert_eq!(
            manager.request_timeout(&entry(json!({ "requestTimeoutMs": 0 }))),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn protocol_version_mode_parsed_from_entry() {
        // TE-D12: ServerEntry.protocolVersion → ProtocolVersionMode
        assert_eq!(
            crate::protocol::parse_protocol_version_mode(entry(json!({})).get("protocolVersion")),
            crate::protocol::ProtocolVersionMode::Legacy
        );
        assert_eq!(
            crate::protocol::parse_protocol_version_mode(
                entry(json!({ "protocolVersion": "legacy" })).get("protocolVersion")
            ),
            crate::protocol::ProtocolVersionMode::Legacy
        );
        assert_eq!(
            crate::protocol::parse_protocol_version_mode(
                entry(json!({ "protocolVersion": "auto" })).get("protocolVersion")
            ),
            crate::protocol::ProtocolVersionMode::Auto
        );
        assert_eq!(
            crate::protocol::parse_protocol_version_mode(
                entry(json!({ "protocolVersion": "2026-07-28" })).get("protocolVersion")
            ),
            crate::protocol::ProtocolVersionMode::Pinned2026
        );
    }

    #[tokio::test]
    async fn oauth_store_token_injected_into_connect_config() {
        use crate::oauth::store::{MemorySecretStore, StoredTokens};

        // A definition that supports_oauth (url, no headers, no static
        // auth: "bearer") + a stored, unexpired token.
        let definition = entry(json!({ "url": "https://a.test/mcp" }));
        let store = crate::oauth::store::OAuthCredentialStore::with_backend(
            Box::new(MemorySecretStore::new()),
            AuthStorageOptions::default(),
        );
        store
            .save_entry(
                "srv",
                crate::oauth::store::AuthEntry {
                    tokens: Some(StoredTokens {
                        access_token: "valid-token".to_string(),
                        expires_at: Some(9999999999.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Some("https://a.test/mcp"),
            )
            .unwrap();

        let (config, _) = http_config_with_auth(&store, "srv", &definition)
            .await
            .expect("config");
        let auth = config
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.clone());
        assert_eq!(auth.as_deref(), Some("Bearer valid-token"));
    }

    #[tokio::test]
    async fn expired_token_without_refresh_not_injected() {
        use crate::oauth::store::{MemorySecretStore, StoredTokens};

        let definition = entry(json!({ "url": "https://a.test/mcp" }));
        let store = crate::oauth::store::OAuthCredentialStore::with_backend(
            Box::new(MemorySecretStore::new()),
            AuthStorageOptions::default(),
        );
        store
            .save_entry(
                "srv",
                crate::oauth::store::AuthEntry {
                    tokens: Some(StoredTokens {
                        access_token: "stale-token".to_string(),
                        expires_at: Some(1.0), // long past
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Some("https://a.test/mcp"),
            )
            .unwrap();

        // resolve_access_token returns None (no refresh token → no
        // metadata discovery for a fake host) → no header injected.
        let (config, _) = http_config_with_auth(&store, "srv", &definition)
            .await
            .expect("config");
        assert!(!config
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization")));
    }

    #[tokio::test]
    async fn explicit_authorization_header_wins_over_oauth_store() {
        let definition = entry(json!({
            "url": "https://a.test/mcp",
            "headers": { "Authorization": "Bearer static" }
        }));
        // No store needed: has headers → supports_oauth is false → no
        // OAuth resolution at all; the static header stays as configured.
        let store = OAuthCredentialStore::with_backend(
            Box::new(crate::oauth::store::MemorySecretStore::new()),
            AuthStorageOptions::default(),
        );
        let (config, _) = http_config_with_auth(&store, "srv", &definition)
            .await
            .expect("config");
        let auth: Vec<_> = config
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .collect();
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].1, "Bearer static");
    }

    /// `shouldFallbackToSse` (server-manager.ts:74-77): 404/405/406/415
    /// fall back to legacy SSE — except for a pinned 2026-07-28 entry,
    /// which never downgrades to the legacy protocol.
    #[test]
    fn sse_fallback_gated_by_protocol_mode() {
        fn should_fallback_to_sse(status: u16, mode: crate::protocol::ProtocolVersionMode) -> bool {
            SSE_FALLBACK_STATUSES.contains(&status) && mode != ProtocolVersionMode::Pinned2026
        }
        assert!(should_fallback_to_sse(404, ProtocolVersionMode::Legacy));
        assert!(should_fallback_to_sse(405, ProtocolVersionMode::Auto));
        // Pinned 2026: 404 must NOT trigger the SSE fallback (upstream
        // shouldFallbackToSse returns false for protocolVersion
        // "2026-07-28").
        assert!(!should_fallback_to_sse(
            404,
            ProtocolVersionMode::Pinned2026
        ));
        assert!(!should_fallback_to_sse(
            415,
            ProtocolVersionMode::Pinned2026
        ));
        assert!(!should_fallback_to_sse(500, ProtocolVersionMode::Legacy));
    }
    // ===== TE24 FR-B: stdio cwd diagnostics (#442) =====

    #[tokio::test]
    async fn stdio_cwd_diagnostics_distinguish_missing_and_not_a_directory() {
        let manager = McpServerManager::new(None);
        let dir = std::env::temp_dir().join("rpi-te24-cwd-diag");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").expect("write");

        // Missing cwd names the path (server-manager.ts:799-803 @ 10a45367).
        let missing = manager
            .connect(
                "srv",
                &entry(json!({ "command": "true", "cwd": "/definitely/not/here" })),
            )
            .await
            .err()
            .expect("connect fails");
        assert_eq!(
            missing.to_string(),
            "MCP server \"srv\" configured cwd does not exist: \"/definitely/not/here\""
        );
        // Non-directory cwd.
        let not_dir = manager
            .connect(
                "srv",
                &entry(json!({ "command": "true", "cwd": file.to_str().unwrap() })),
            )
            .await
            .err()
            .expect("connect fails");
        assert!(
            not_dir
                .to_string()
                .starts_with("MCP server \"srv\" configured cwd is not a directory: \""),
            "got: {not_dir}"
        );
        // The default (session) cwd is diagnosed too, not only an explicit one.
        let manager_default = McpServerManager::new(Some(file.to_str().unwrap().to_string()));
        let by_default = manager_default
            .connect("srv", &entry(json!({ "command": "true" })))
            .await
            .err()
            .expect("connect fails");
        assert!(
            by_default
                .to_string()
                .contains("configured cwd is not a directory"),
            "got: {by_default}"
        );
        // A valid directory cwd proceeds to the spawn (a different error:
        // the handshake fails because `true` is not an MCP server).
        let ok_cwd = manager
            .connect(
                "srv",
                &entry(json!({ "command": "true", "cwd": dir.to_str().unwrap() })),
            )
            .await
            .err()
            .expect("connect fails");
        assert!(
            !ok_cwd.to_string().contains("configured cwd"),
            "valid cwd must not trip the diagnostic: {ok_cwd}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== TE24 FR-B: probe classification flavors (#415) =====

    #[test]
    fn probe_not_mcp_status_flavors() {
        // mcp-probe.ts:143-156 @ 10a45367.
        assert_eq!(
            not_mcp(&503, "text/html"),
            "endpoint returned HTML (503) — server is temporarily unavailable; \
MCP endpoint shape could not be determined"
        );
        assert_eq!(
            not_mcp(&202, "application/json"),
            "endpoint returned application/json (202) — MCP endpoint shape could not be determined"
        );
        assert_eq!(
            not_mcp(&401, ""),
            "endpoint returned an untyped response (401) — authentication may \
be required; MCP endpoint shape could not be determined"
        );
        assert_eq!(
            not_mcp(&404, "text/html"),
            "endpoint returned HTML (404) — this URL does not appear to speak MCP"
        );
    }

    #[test]
    fn probe_ambiguous_statuses_only_cover_202_401_503() {
        // mcp-probe.ts:158-160 @ 10a45367.
        for status in [202u16, 401, 503] {
            assert!(ambiguous_not_mcp(&status, "text/html").is_some());
        }
        for status in [200u16, 400, 404, 500] {
            assert!(ambiguous_not_mcp(&status, "text/html").is_none());
        }
    }

    #[test]
    fn probe_transient_503_classification() {
        // #411/#424: a 503 stays an availability error.
        assert!(is_transient_http_connect_error(&ProtocolError::Http {
            status: 503,
            message: "Error POSTing to endpoint".to_string()
        }));
        assert!(!is_transient_http_connect_error(&ProtocolError::Http {
            status: 500,
            message: "Error POSTing to endpoint".to_string()
        }));
        assert!(!is_transient_http_connect_error(
            &ProtocolError::Unauthorized
        ));
    }
}
