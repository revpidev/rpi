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
    pub tools: Vec<Value>,
    pub resources: Vec<Value>,
    pub prompts: Vec<Value>,
    pub prompt_discovery_failed: bool,
    pub instructions: Option<String>,
    pub last_used_at: AtomicU64,
    pub in_flight: AtomicUsize,
    pub status: Mutex<ConnectionStatus>,
    pub credentials_invalidated: AtomicBool,
}

impl ServerConnection {
    pub fn status(&self) -> ConnectionStatus {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
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
pub struct McpServerManager {
    connections: Mutex<HashMap<String, Arc<ServerConnection>>>,
    connect_promises: Mutex<HashMap<String, SharedConnect>>,
    default_cwd: Option<String>,
    default_request_timeout: Mutex<Option<Duration>>,
    runtime_cancel: Mutex<CancellationToken>,
    stopped: AtomicBool,
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
        })
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
        let store = OAuthCredentialStore::new(AuthStorageOptions::default());
        let config = http_config_with_auth(&store, name, definition).await?;
        let supports_oauth = supports_oauth(definition);
        let mode = crate::protocol::parse_protocol_version_mode(definition.get("protocolVersion"));

        match self
            .try_streamable(name, definition, config, request_timeout)
            .await
        {
            Ok(connection) => Ok(connection),
            Err(error @ ProtocolError::Unauthorized) => {
                if supports_oauth {
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
                let config = http_config_with_auth(&store, name, definition).await?;
                match self
                    .try_sse(name, definition, config, request_timeout)
                    .await
                {
                    Ok(connection) => Ok(connection),
                    Err(error @ ProtocolError::Unauthorized) => {
                        if supports_oauth {
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
) -> Result<HttpConfig, ProtocolError> {
    let mut config = resolve_http_config_with_server(definition, name)?;
    if supports_oauth(definition) {
        inject_oauth_authorization(store, name, definition, &mut config).await;
    }
    Ok(config)
}

/// Inject `Authorization: Bearer <token>` into `config.headers` when the
/// store holds a usable access token for this server+URL. Store failures
/// degrade to an unauthenticated connect — the 401 → needs-auth flow
/// surfaces the auth requirement to the caller.
async fn inject_oauth_authorization(
    store: &OAuthCredentialStore,
    name: &str,
    definition: &ServerEntry,
    config: &mut HttpConfig,
) {
    if config
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    {
        return; // an explicit Authorization header always wins
    }
    let Some(server_url) = crate::utils::resolve_server_url(definition.get("url"))
        .ok()
        .flatten()
    else {
        return;
    };
    match crate::oauth::resolve_access_token(store, name, &server_url, definition).await {
        Ok(Some(token)) => {
            config
                .headers
                .push(("Authorization".to_string(), format!("Bearer {token}")));
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(server = name, %error, "OAuth credential resolution failed");
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
        tools: metadata.tools,
        resources: metadata.resources,
        prompts: metadata.prompts,
        prompt_discovery_failed: metadata.prompt_discovery_failed,
        last_used_at: AtomicU64::new(now_ms()),
        in_flight: AtomicUsize::new(0),
        status: Mutex::new(ConnectionStatus::Connected),
        credentials_invalidated: AtomicBool::new(false),
    }
}

/// server-manager.ts:495-507 — the needs-auth placeholder connection.
fn needs_auth_connection(definition: &ServerEntry) -> ServerConnection {
    ServerConnection {
        client: None,
        definition: definition.clone(),
        tools: Vec::new(),
        resources: Vec::new(),
        prompts: Vec::new(),
        prompt_discovery_failed: false,
        instructions: None,
        last_used_at: AtomicU64::new(now_ms()),
        in_flight: AtomicUsize::new(0),
        status: Mutex::new(ConnectionStatus::NeedsAuth),
        credentials_invalidated: AtomicBool::new(true),
    }
}

/// `probeMcpEndpoint` (mcp-probe.ts:172-186): one unauthenticated
/// metadata-only request to classify an HTTP endpoint's protocol shape.
/// Returns a human-readable classification string.
///
/// Port of the three-stage probe strategy: modern (`server/discover` +
/// `2026-07-28`) → legacy-post (`initialize`) → legacy-sse (GET stream).
/// TE-D05: enriches HTTP connection failure error messages.
async fn probe_mcp_endpoint(url: &str) -> Option<String> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
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

    let classification = classify_probe_response(modern_response, true).await;
    if let Some(classification) = classification {
        return Some(classification);
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
                    "clientInfo": { "name": "rpi-mcp-probe", "version": "1.0.0" }
                }
            })
            .to_string(),
        )
        .send()
        .await
        .ok()?;

    let classification = classify_probe_response(legacy_response, false).await;
    if let Some(classification) = classification {
        return Some(classification);
    }

    // Stage 3: legacy SSE (GET stream)
    let sse_response = client
        .get(url)
        .header("accept", "text/event-stream")
        .send()
        .await
        .ok()?;

    let content_type = sse_response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if sse_response.status().is_success() && content_type.starts_with("text/event-stream") {
        return Some("endpoint responded with an MCP event stream".to_string());
    }

    Some(format!(
        "endpoint returned {} ({}) — this URL does not appear to speak MCP",
        probe_response_kind(content_type),
        sse_response.status().as_u16()
    ))
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

/// `classifyResponse` (mcp-probe.ts:121-155): check if the response
/// indicates an MCP endpoint. Consumes the response body. Returns
/// `None` for the "unrecognized" outcome (caller falls through to the next
/// probe stage).
async fn classify_probe_response(response: reqwest::Response, is_modern: bool) -> Option<String> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    // SSE stream → MCP
    if response.status().is_success() && content_type.starts_with("text/event-stream") {
        return Some("endpoint responded with an MCP event stream".to_string());
    }

    // JSON-RPC envelope check (allowJson strategies; 401 also inspected for
    // the Bearer-challenge classification).
    let status = response.status().as_u16();
    let bearer_challenge = is_bearer_challenge(&response);
    if response.status().is_success() || status == 401 {
        if let Ok(text) = response.text().await {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if value.get("jsonrpc") == Some(&json!("2.0")) {
                    if let Some(result) = value.get("result") {
                        // Modern stage: an error envelope or a mismatched
                        // protocolVersion is "unsupported-modern" (fall
                        // through to the legacy stages).
                        if is_modern
                            && result.get("protocolVersion").and_then(Value::as_str)
                                != Some(crate::oauth::MODERN_PROTOCOL_VERSION)
                        {
                            return None;
                        }
                        return Some(if is_modern {
                            "endpoint supports stateless MCP 2026-07-28 server/discover".to_string()
                        } else {
                            "endpoint responded with a JSON-RPC 2.0 envelope".to_string()
                        });
                    }
                    if value.get("error").is_some() {
                        if is_modern {
                            return None; // unsupported-modern
                        }
                        if status == 401 && bearer_challenge {
                            return Some(
                                "endpoint requires Bearer authentication and responded with a \
                                 JSON-RPC 2.0 error"
                                    .to_string(),
                            );
                        }
                        return None;
                    }
                }
            }
        }
    }

    None
}

/// `enrichHttpConnectionError` (server-manager.ts:522-530): append a
/// probe-based classification suffix to the HTTP connection failure message.
/// The probe itself must NOT carry credentials — it is a metadata-only
/// unauthenticated request (G4 red line).
async fn enrich_http_connection_error(
    definition: &ServerEntry,
    error: ProtocolError,
) -> ProtocolError {
    let original_message = error.to_string();
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

        let config = http_config_with_auth(&store, "srv", &definition)
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
        let config = http_config_with_auth(&store, "srv", &definition)
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
        let config = http_config_with_auth(&store, "srv", &definition)
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
}
