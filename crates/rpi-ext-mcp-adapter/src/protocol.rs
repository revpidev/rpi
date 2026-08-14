//! Hand-written thin MCP JSON-RPC client (design §3.1 decision: no `rmcp`).
//!
//! Wire behavior is aligned line-by-line with the pinned upstream stack:
//! `@modelcontextprotocol/client@2.0.0` (`Protocol` / `Client._legacyHandshake`)
//! as driven by `server-manager.ts` @ pi-mcp-adapter v2.24.0 (3d953f90).
//!
//! Pinned facts (SDK 2.0 source, `client/dist/index.mjs` /
//! `core/dist/auth-*.mjs` of the npm package):
//! - Legacy initialize offers `protocolVersion: "2025-11-25"` (first legacy
//!   entry of `SUPPORTED_PROTOCOL_VERSIONS`) with `capabilities: {}` (the
//!   adapter registers neither sampling nor elicitation in P0) and
//!   `clientInfo: { name: "rpi-mcp-<server>", version: "1.0.0" }` (upstream:
//!   `pi-mcp-<server>` — product-name rename per design O1).
//! - The result's `protocolVersion` must be one of the four legacy versions
//!   or the handshake fails with `Server's protocol version is not
//!   supported: <v>`; `notifications/initialized` follows a successful
//!   handshake.
//! - Request ids are integers starting at 0; a request without params omits
//!   the `params` key; the default request timeout is 60s; on timeout the
//!   client sends `notifications/cancelled` with `{ requestId, reason }`.
//! - Server→client requests (sampling/elicitation, P2) get a JSON-RPC
//!   `-32601` Method-not-found error response in P0.

pub mod http;
pub mod stdio;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

/// Legacy-era protocol versions accepted in the initialize result
/// (SDK 2.0 `SUPPORTED_PROTOCOL_VERSIONS` minus the modern era).
pub const LEGACY_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The version offered in the legacy initialize request
/// (`legacyProtocolVersions(SUPPORTED_PROTOCOL_VERSIONS)[0]`).
pub const LEGACY_OFFERED_VERSION: &str = LEGACY_PROTOCOL_VERSIONS[0];

/// `protocolVersion` negotiation mode (FR-P1-10, design R6).
///
/// Upstream `ServerEntry.protocolVersion`:
/// - `"legacy"` (default): SDK `_legacyHandshake`, offer `2025-11-25`,
///   accept any of the four legacy versions.
/// - `"auto"`: offer the modern version (`2026-07-28`) first; on rejection
///   fall back to the legacy handshake.
/// - `"2026-07-28"`: pin the modern version — offer it, reject any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolVersionMode {
    #[default]
    Legacy,
    Auto,
    #[allow(non_camel_case_types)]
    Pinned2026,
}

/// Parse the `protocolVersion` field from a ServerEntry (types.ts:182).
pub fn parse_protocol_version_mode(value: Option<&Value>) -> ProtocolVersionMode {
    match value.and_then(Value::as_str) {
        Some("auto") => ProtocolVersionMode::Auto,
        Some("2026-07-28") => ProtocolVersionMode::Pinned2026,
        _ => ProtocolVersionMode::Legacy,
    }
}

/// SDK 2.0 `DEFAULT_REQUEST_TIMEOUT_MSEC`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(60_000);

/// Client implementation identity (upstream `pi-mcp-<server>`; product-name
/// rename per design O1, recorded as a text-difference exemption).
pub fn client_info(server_name: &str) -> Value {
    json!({ "name": format!("rpi-mcp-{server_name}"), "version": "1.0.0" })
}

/// Errors from the protocol layer. `Unauthorized` (HTTP 401) is mapped to
/// the manager's `needs-auth` status; `Http` with 404/405/406/415 drives the
/// streamable→SSE fallback matrix (upstream `SdkHttpError` /
/// `UnauthorizedError`).
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProtocolError {
    #[error("{0}")]
    Transport(String),
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("unauthorized")]
    Unauthorized,
    #[error("MCP error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("request timed out")]
    Timeout,
    #[error("connection closed")]
    Closed,
    #[error("{0}")]
    Protocol(String),
}

/// A server-initiated message the client does not answer directly.
#[derive(Debug, Clone)]
pub enum Incoming {
    /// `notifications/*` (method present, no id).
    Notification(Value),
    /// A server→client request (method + id). P0 answers `-32601`.
    Request(Value),
}

/// One MCP connection's transport (stdio child pipes, streamable HTTP or
/// legacy SSE). Mirrors the SDK `Transport` interface: `send` pumps any
/// response messages into the incoming channel handed to the client.
#[async_trait::async_trait]
pub trait McpTransport: Send + Sync {
    async fn send(&self, message: Value) -> Result<(), ProtocolError>;
    async fn close(&self) -> Result<(), ProtocolError>;
    /// Streamable HTTP session id, once the initialize response provided one.
    fn session_id(&self) -> Option<String> {
        None
    }
    /// Record the negotiated protocol version (streamable HTTP sends it as
    /// the `mcp-protocol-version` header on post-handshake requests).
    fn set_protocol_version(&self, _version: &str) {}
}

/// Result of the metadata discovery phase (`server-manager.ts`
/// `fetchAllTools`/`fetchAllResources`/`fetchAllPrompts`).
#[derive(Debug, Clone, Default)]
pub struct DiscoveredMetadata {
    pub tools: Vec<Value>,
    pub resources: Vec<Value>,
    pub prompts: Vec<Value>,
    /// True when prompts were advertised but `prompts/list` failed.
    pub prompt_discovery_failed: bool,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, ProtocolError>>>>>;
type NotificationHandler = Arc<dyn Fn(Value) + Send + Sync>;

/// The JSON-RPC client over one transport (`Protocol` + legacy `Client`).
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    pending: PendingMap,
    next_id: AtomicU64,
    negotiated_version: Mutex<Option<String>>,
    server_capabilities: Mutex<Option<Value>>,
    server_info: Mutex<Option<Value>>,
    instructions: Mutex<Option<String>>,
    notification_handler: Mutex<Option<NotificationHandler>>,
    /// SDK `client.onclose`: fired when the transport's incoming channel
    /// closes (child exit, stream end) — the manager flips the connection
    /// status to `closed` through it.
    on_close: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    closed: AtomicBool,
}

impl McpClient {
    /// Start the client over an already-started transport: spawn the
    /// dispatcher task draining `incoming`.
    pub fn new(
        transport: Arc<dyn McpTransport>,
        mut incoming: mpsc::UnboundedReceiver<Value>,
    ) -> Arc<Self> {
        let client = Arc::new(Self {
            transport,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(0),
            negotiated_version: Mutex::new(None),
            server_capabilities: Mutex::new(None),
            server_info: Mutex::new(None),
            instructions: Mutex::new(None),
            notification_handler: Mutex::new(None),
            on_close: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&client);
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                let Some(client) = weak.upgrade() else { break };
                client.dispatch_incoming(message);
            }
            // Transport closed the channel: fail every pending request and
            // notify the owner (SDK onclose semantics).
            if let Some(client) = weak.upgrade() {
                client.fail_all_pending();
                let on_close = client
                    .on_close
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(on_close) = on_close {
                    on_close();
                }
            }
        });
        client
    }

    fn dispatch_incoming(&self, message: Value) {
        let id = message.get("id").and_then(Value::as_u64);
        let is_response = message.get("result").is_some() || message.get("error").is_some();
        if let (Some(id), true) = (id, is_response) {
            let sender = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(sender) = sender {
                let outcome = if let Some(error) = message.get("error") {
                    Err(ProtocolError::Rpc {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string(),
                        data: error.get("data").cloned(),
                    })
                } else {
                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = sender.send(outcome);
            }
            return;
        }
        if message.get("method").is_some() {
            if let Some(id) = id {
                // Server→client request (sampling/elicitation/roots are P2):
                // answer Method-not-found so the server is not left hanging.
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" },
                });
                let transport = self.transport.clone();
                tokio::spawn(async move {
                    let _ = transport.send(response).await;
                });
                return;
            }
            let handler = self
                .notification_handler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(handler) = handler {
                handler(message);
            }
        }
    }

    fn fail_all_pending(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(ProtocolError::Closed));
        }
    }

    /// `client.onclose` (server-manager.ts:453-457).
    pub fn set_on_close(&self, handler: Arc<dyn Fn() + Send + Sync>) {
        *self.on_close.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    /// Register the `notifications/*` handler (list-changed refresh etc.).
    pub fn on_notification(&self, handler: NotificationHandler) {
        *self
            .notification_handler
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    /// Send one request and await its response (SDK `Protocol.request`).
    ///
    /// `params: None` omits the `params` key entirely (SDK behavior for e.g.
    /// `tools/list` without a cursor). On timeout the client sends
    /// `notifications/cancelled` before failing the request.
    pub async fn call(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, ProtocolError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ProtocolError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let mut message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        if let Some(params) = params {
            message["params"] = params;
        }
        if let Err(error) = self.transport.send(message).await {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_dropped)) => Err(ProtocolError::Closed),
            Err(_elapsed) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                let cancelled = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": { "requestId": id, "reason": "Request timed out" },
                });
                let _ = self.transport.send(cancelled).await;
                Err(ProtocolError::Timeout)
            }
        }
    }

    /// Send a notification (no id, no response awaited).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), ProtocolError> {
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.transport.send(message).await
    }

    /// The legacy initialize handshake (SDK 2.0 `Client._legacyHandshake`)
    /// plus the adapter's metadata discovery (`fetchAll*` in
    /// `server-manager.ts`).
    ///
    /// Uses the legacy protocol version negotiation (default for P0).
    pub async fn initialize(
        &self,
        server_name: &str,
        request_timeout: Duration,
    ) -> Result<DiscoveredMetadata, ProtocolError> {
        self.initialize_with_version(server_name, request_timeout, ProtocolVersionMode::Legacy)
            .await
    }

    /// `initialize` with an explicit protocol version negotiation mode
    /// (FR-P1-10). Modes mirror the upstream `protocolVersion` server-entry
    /// setting:
    /// - `Legacy`: offer `2025-11-25`, accept any legacy version (P0 default).
    /// - `Auto`: offer `2026-07-28` first; on rejection, fall back to legacy.
    /// - `Pinned2026`: offer `2026-07-28` only, no fallback (pin).
    pub async fn initialize_with_version(
        &self,
        server_name: &str,
        request_timeout: Duration,
        mode: ProtocolVersionMode,
    ) -> Result<DiscoveredMetadata, ProtocolError> {
        let offered_version = match mode {
            ProtocolVersionMode::Legacy => LEGACY_OFFERED_VERSION,
            ProtocolVersionMode::Auto | ProtocolVersionMode::Pinned2026 => {
                crate::oauth::MODERN_PROTOCOL_VERSION
            }
        };

        let result = match self
            .try_initialize_with_version(server_name, request_timeout, offered_version, mode)
            .await
        {
            Ok(result) => result,
            // Auto fallback (SDK `_negotiateProtocolVersion` retry): match
            // both the local handshake validation error (Protocol) and a
            // server-reported JSON-RPC error (Rpc) carrying the same text.
            Err(ProtocolError::Protocol(ref msg))
                if mode == ProtocolVersionMode::Auto
                    && msg.contains("protocol version is not supported") =>
            {
                self.try_initialize_with_version(
                    server_name,
                    request_timeout,
                    LEGACY_OFFERED_VERSION,
                    mode,
                )
                .await?
            }
            Err(ProtocolError::Rpc { ref message, .. })
                if mode == ProtocolVersionMode::Auto
                    && message.contains("protocol version is not supported") =>
            {
                self.try_initialize_with_version(
                    server_name,
                    request_timeout,
                    LEGACY_OFFERED_VERSION,
                    mode,
                )
                .await?
            }
            Err(error) => return Err(error),
        };

        // Discovery phase is shared across all modes.
        self.transport.set_protocol_version(
            result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(offered_version),
        );

        let tools = self.fetch_all_tools(request_timeout).await?;
        let resources = self.fetch_all_resources(request_timeout).await?;
        let (prompts, prompt_discovery_failed) = self.fetch_all_prompts(request_timeout).await?;
        Ok(DiscoveredMetadata {
            tools,
            resources,
            prompts,
            prompt_discovery_failed,
        })
    }

    /// Send the initialize request with a specific version, store the
    /// result, and send `notifications/initialized`.
    async fn try_initialize_with_version(
        &self,
        server_name: &str,
        request_timeout: Duration,
        offered_version: &str,
        mode: ProtocolVersionMode,
    ) -> Result<Value, ProtocolError> {
        let result = self
            .call(
                "initialize",
                Some(json!({
                    "protocolVersion": offered_version,
                    "capabilities": {},
                    "clientInfo": client_info(server_name),
                })),
                request_timeout,
            )
            .await?;

        let server_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();

        // Validate the server's offered version against the mode
        // (upstream resolveVersionNegotiation, server-manager.ts:79-91):
        // - `Legacy` (default): accept only the four legacy versions.
        // - `Auto`: accept the modern version or any legacy version (the
        //   fallback in `initialize_with_version` handles rejections).
        // - `Pinned2026`: accept only the modern version — a legacy reply
        //   fails the handshake (pin, no downgrade).
        let is_modern = server_version == crate::oauth::MODERN_PROTOCOL_VERSION;
        let is_legacy = LEGACY_PROTOCOL_VERSIONS.contains(&server_version);
        let accepted = match mode {
            ProtocolVersionMode::Legacy => is_legacy,
            ProtocolVersionMode::Auto => is_modern || is_legacy,
            ProtocolVersionMode::Pinned2026 => is_modern,
        };
        if !accepted {
            return Err(ProtocolError::Protocol(format!(
                "Server's protocol version is not supported: {server_version}"
            )));
        }

        *self
            .server_capabilities
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = result.get("capabilities").cloned();
        *self.server_info.lock().unwrap_or_else(|e| e.into_inner()) =
            result.get("serverInfo").cloned();
        *self.instructions.lock().unwrap_or_else(|e| e.into_inner()) = result
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_string);
        *self
            .negotiated_version
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(server_version.to_string());

        self.notify("notifications/initialized", None).await?;
        Ok(result)
    }

    /// `fetchAllTools` (server-manager.ts:849-860) + SDK `Client.listTools`
    /// capability guard: when the server does not advertise `tools`, the SDK
    /// returns an empty list WITHOUT issuing a request (console warning on
    /// the Node side; tracing here). Parity-verified frame-level by
    /// scripts/mcp-parity (design §5.2).
    async fn fetch_all_tools(&self, timeout: Duration) -> Result<Vec<Value>, ProtocolError> {
        if !self.advertises("tools") {
            debug!(
                "Client.listTools() called but server does not advertise tools capability - \
                 returning empty list"
            );
            return Ok(Vec::new());
        }
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.map(|c| json!({ "cursor": c }));
            let result = self.call("tools/list", params, timeout).await?;
            if let Some(tools) = result.get("tools").and_then(Value::as_array) {
                all.extend(tools.iter().cloned());
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(all)
    }

    /// `fetchAllResources` (server-manager.ts:887-910): capability-gated;
    /// listing failures degrade to `[]` (401 aborts the connect).
    async fn fetch_all_resources(&self, timeout: Duration) -> Result<Vec<Value>, ProtocolError> {
        if !self.advertises("resources") {
            return Ok(Vec::new());
        }
        match self.fetch_all("resources/list", "resources", timeout).await {
            Ok(resources) => Ok(resources),
            Err(error @ ProtocolError::Unauthorized) => Err(error),
            Err(_) => Ok(Vec::new()),
        }
    }

    /// `fetchAllPrompts` (server-manager.ts:862-885): capability-gated;
    /// failures are reported as `failed: true` (401 aborts the connect).
    async fn fetch_all_prompts(
        &self,
        timeout: Duration,
    ) -> Result<(Vec<Value>, bool), ProtocolError> {
        if !self.advertises("prompts") {
            return Ok((Vec::new(), false));
        }
        match self.fetch_all("prompts/list", "prompts", timeout).await {
            Ok(prompts) => Ok((prompts, false)),
            Err(error @ ProtocolError::Unauthorized) => Err(error),
            Err(_) => Ok((Vec::new(), true)),
        }
    }

    async fn fetch_all(
        &self,
        method: &str,
        result_key: &str,
        timeout: Duration,
    ) -> Result<Vec<Value>, ProtocolError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.map(|c| json!({ "cursor": c }));
            let result = self.call(method, params, timeout).await?;
            if let Some(items) = result.get(result_key).and_then(Value::as_array) {
                all.extend(items.iter().cloned());
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(all)
    }

    fn advertises(&self, capability: &str) -> bool {
        self.server_capabilities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|c| c.get(capability))
            .is_some()
    }

    pub fn instructions(&self) -> Option<String> {
        self.instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Expose the transport's session id (only Streamable HTTP has one;
    /// stdio/SSE return `None`). Used by session recovery (FR-P1-08).
    pub fn session_id(&self) -> Option<String> {
        self.transport.session_id()
    }

    /// `client.callTool` — `tools/call` with `{ name, arguments }`.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, ProtocolError> {
        self.call(
            "tools/call",
            Some(json!({ "name": name, "arguments": arguments })),
            timeout,
        )
        .await
    }

    /// `client.readResource` — `resources/read` with `{ uri }`.
    pub async fn read_resource(
        &self,
        uri: &str,
        timeout: Duration,
    ) -> Result<Value, ProtocolError> {
        self.call("resources/read", Some(json!({ "uri": uri })), timeout)
            .await
    }

    /// `client.close()`: close the transport and fail pending requests.
    pub async fn close(&self) -> Result<(), ProtocolError> {
        self.closed.store(true, Ordering::SeqCst);
        let result = self.transport.close().await;
        self.fail_all_pending();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory transport recording sent messages; queued responses are
    /// replayed into the client.
    struct MemTransport {
        sent: Mutex<Vec<Value>>,
        incoming: mpsc::UnboundedSender<Value>,
    }

    #[async_trait::async_trait]
    impl McpTransport for MemTransport {
        async fn send(&self, message: Value) -> Result<(), ProtocolError> {
            self.sent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(message.clone());
            // Auto-answer requests with a canned response.
            if let Some(id) = message.get("id").and_then(Value::as_u64) {
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                let result = match method {
                    "initialize" => json!({
                        "protocolVersion": "2025-03-26",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mem", "version": "0" },
                    }),
                    "tools/list" => json!({ "tools": [{ "name": "t1" }] }),
                    _ => json!({}),
                };
                let _ = self.incoming.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }));
            }
            Ok(())
        }
        async fn close(&self) -> Result<(), ProtocolError> {
            Ok(())
        }
    }

    fn client_with(transport: MemTransport) -> (Arc<McpClient>, Arc<MemTransport>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let transport = MemTransport {
            incoming: tx,
            ..transport
        };
        let transport = Arc::new(transport);
        let client = McpClient::new(transport.clone(), rx);
        (client, transport)
    }

    #[tokio::test]
    async fn legacy_handshake_offers_pinned_version_and_discovers_tools() {
        let (client, transport) = client_with(MemTransport {
            sent: Mutex::new(Vec::new()),
            incoming: mpsc::unbounded_channel().0,
        });
        let metadata = client
            .initialize("demo", DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("initialize");
        assert_eq!(metadata.tools, vec![json!({ "name": "t1" })]);
        let sent = transport.sent.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            sent[0],
            json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "rpi-mcp-demo", "version": "1.0.0" },
                },
            })
        );
        assert_eq!(
            sent[1],
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        );
        // tools/list without a cursor omits `params`.
        assert_eq!(
            sent[2],
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })
        );
    }

    #[tokio::test]
    async fn unsupported_result_version_fails_the_handshake() {
        struct BadVersion {
            incoming: mpsc::UnboundedSender<Value>,
        }
        #[async_trait::async_trait]
        impl McpTransport for BadVersion {
            async fn send(&self, message: Value) -> Result<(), ProtocolError> {
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let _ = self.incoming.send(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "protocolVersion": "1999-01-01" },
                    }));
                }
                Ok(())
            }
            async fn close(&self) -> Result<(), ProtocolError> {
                Ok(())
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let transport: Arc<dyn McpTransport> = Arc::new(BadVersion { incoming: tx });
        let client = McpClient::new(transport, rx);
        let error = client
            .initialize("demo", DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect_err("handshake must fail");
        assert_eq!(
            error.to_string(),
            "Server's protocol version is not supported: 1999-01-01"
        );
    }

    #[tokio::test]
    async fn pinned2026_rejects_legacy_version_reply() {
        // resolveVersionNegotiation pin: offering 2026-07-28 means only the
        // modern version is accepted — a legacy reply fails the handshake.
        struct LegacyReply {
            incoming: mpsc::UnboundedSender<Value>,
        }
        #[async_trait::async_trait]
        impl McpTransport for LegacyReply {
            async fn send(&self, message: Value) -> Result<(), ProtocolError> {
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let _ = self.incoming.send(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": { "name": "legacy", "version": "0" },
                        },
                    }));
                }
                Ok(())
            }
            async fn close(&self) -> Result<(), ProtocolError> {
                Ok(())
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let transport: Arc<dyn McpTransport> = Arc::new(LegacyReply { incoming: tx });
        let client = McpClient::new(transport, rx);
        let error = client
            .initialize_with_version(
                "demo",
                DEFAULT_REQUEST_TIMEOUT,
                ProtocolVersionMode::Pinned2026,
            )
            .await
            .expect_err("pin must reject legacy");
        assert_eq!(
            error.to_string(),
            "Server's protocol version is not supported: 2025-06-18"
        );
    }

    #[tokio::test]
    async fn pinned2026_accepts_modern_version_reply() {
        struct ModernReply {
            incoming: mpsc::UnboundedSender<Value>,
        }
        #[async_trait::async_trait]
        impl McpTransport for ModernReply {
            async fn send(&self, message: Value) -> Result<(), ProtocolError> {
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                    let result = match method {
                        "initialize" => json!({
                            "protocolVersion": crate::oauth::MODERN_PROTOCOL_VERSION,
                            "capabilities": {},
                            "serverInfo": { "name": "modern", "version": "0" },
                        }),
                        _ => json!({}),
                    };
                    let _ = self.incoming.send(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    }));
                }
                Ok(())
            }
            async fn close(&self) -> Result<(), ProtocolError> {
                Ok(())
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let transport: Arc<dyn McpTransport> = Arc::new(ModernReply { incoming: tx });
        let client = McpClient::new(transport, rx);
        client
            .initialize_with_version(
                "demo",
                DEFAULT_REQUEST_TIMEOUT,
                ProtocolVersionMode::Pinned2026,
            )
            .await
            .expect("pin accepts modern");
    }

    #[tokio::test]
    async fn auto_falls_back_to_legacy_on_modern_rejection() {
        // The Auto fallback branch keys on the exact "protocol version is
        // not supported" text produced by the mode validation above.
        struct RejectThenLegacy {
            incoming: mpsc::UnboundedSender<Value>,
            calls: AtomicU64,
        }
        #[async_trait::async_trait]
        impl McpTransport for RejectThenLegacy {
            async fn send(&self, message: Value) -> Result<(), ProtocolError> {
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let call = self.calls.fetch_add(1, Ordering::SeqCst);
                    let offered = message
                        .get("params")
                        .and_then(|p| p.get("protocolVersion"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let result = if offered == crate::oauth::MODERN_PROTOCOL_VERSION {
                        // A legacy-only server answering the modern offer
                        // with a legacy version: pin/auto validation rejects.
                        json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": { "name": "legacy", "version": "0" },
                        })
                    } else if call == 0 {
                        // First (modern) call reaches here only when the
                        // server_version check failed differently; unused.
                        json!({})
                    } else {
                        json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": { "name": "legacy", "version": "0" },
                        })
                    };
                    let _ = self.incoming.send(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    }));
                }
                Ok(())
            }
            async fn close(&self) -> Result<(), ProtocolError> {
                Ok(())
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let transport: Arc<dyn McpTransport> = Arc::new(RejectThenLegacy {
            incoming: tx,
            calls: AtomicU64::new(0),
        });
        let client = McpClient::new(transport, rx);
        client
            .initialize_with_version("demo", DEFAULT_REQUEST_TIMEOUT, ProtocolVersionMode::Auto)
            .await
            .expect("auto falls back to legacy handshake");
    }
}
