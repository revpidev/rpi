//! HTTP transports: streamable HTTP first, legacy SSE fallback on the
//! 404/405/406/415 matrix, 401 → `Unauthorized` (mapped to `needs-auth` by
//! the manager) (FR-P0-08, design §3.1).
//!
//! Port of the SDK 2.0 `StreamableHTTPClientTransport` /
//! `SSEClientTransport` behavior as driven by `server-manager.ts`
//! (`connectHttpClient` / `shouldFallbackToSse`) @ pi-mcp-adapter v2.24.0
//! (3d953f90). Pinned details (SDK source):
//! - POST with `content-type: application/json` and `accept:
//!   application/json, text/event-stream` (merged with configured headers);
//!   `mcp-session-id` is omitted on the handshake and captured from the
//!   initialize response; `mcp-protocol-version` rides post-handshake
//!   requests once negotiated.
//! - 202 → accepted, no body; a `notifications/initialized` 202 additionally
//!   opens the standalone GET SSE stream (405 there means "no stream").
//! - Responses to requests arrive either as `application/json` (single
//!   message or array) or as an SSE stream on the POST response.
//! - Legacy SSE: GET `accept: text/event-stream`, wait for the `endpoint`
//!   event (same-origin enforced), POST messages to that endpoint expecting
//!   2xx; incoming `message` events carry the JSON-RPC payloads.
//! - 401 without an auth provider → `UnauthorizedError`
//!   ([`ProtocolError::Unauthorized`]); OAuth itself is P1.
//!
//! P0 scope cuts: SSE stream auto-reconnect/backoff (SDK reconnection
//! options) and session recovery (FR-P1-08) are not implemented; the
//! standalone GET stream just ends. `httpTransport: "sse"` pinning (Agent
//! Plugins) is accepted but P2-owned.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::{McpTransport, ProtocolError};
use crate::metadata::ServerEntry;

/// HTTP statuses that trigger the streamable → legacy SSE fallback
/// (`shouldFallbackToSse`, server-manager.ts:74-77).
pub const SSE_FALLBACK_STATUSES: [u16; 4] = [404, 405, 406, 415];

/// Incremental SSE event decoder (`data:` aggregation, `event:`/`id:`
/// fields, comment lines, blank-line dispatch). The `retry:` field updates
/// the caller-visible reconnection delay hint (SDK `EventSourceParserStream`
/// `onRetry`). Input is raw bytes: lines are split on the `\n` byte (0x0A
/// never appears inside a UTF-8 multibyte sequence) and each complete line
/// is decoded separately, so code points split across network read
/// boundaries survive intact (SDK pipes the body through a streaming
/// `TextDecoderStream` before the parser).
#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event_type: Option<String>,
    data: Vec<String>,
    id: Option<String>,
    /// A line exceeded `MAX_LINE_BYTES`: the current event is discarded and
    /// bytes are skipped until the next line boundary.
    skipping: bool,
    /// Last `retry:` field seen (milliseconds), SDK `_serverRetryMs`.
    pub retry_ms: Option<u64>,
}

/// Cap on a single SSE line's byte length — a malicious server cannot grow
/// the decoder buffer without bound. Aligned with the SDK stdio ReadBuffer
/// `maxBufferSize` (10 MiB); the SDK's EventSource parser itself is
/// unbounded. NOTE: legitimate responses (tools/list of a large server,
/// big tool results) routinely exceed a few KiB on ONE `data:` line — a
/// KiB-scale cap silently discards them (regression: tavily tools/list).
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

/// One decoded SSE event.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; returns the events completed within it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
            let line = line.trim_end_matches('\r');
            if self.skipping {
                // Tail of an oversized line: drop it, resume at this boundary.
                self.skipping = false;
                continue;
            }
            if line.is_empty() {
                if let Some(event) = self.dispatch() {
                    events.push(event);
                }
                continue;
            }
            if line.len() > MAX_LINE_BYTES {
                self.drop_pending_event();
                continue;
            }
            if let Some(rest) = line.strip_prefix(':') {
                let _ = rest; // SSE comment
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            match field {
                "data" => self.data.push(value.to_string()),
                "event" => self.event_type = Some(value.to_string()),
                "id" => self.id = Some(value.to_string()),
                "retry" => {
                    // Non-numeric retry values are ignored (EventSource
                    // semantics: only whole milliseconds apply).
                    if let Ok(ms) = value.parse::<u64>() {
                        self.retry_ms = Some(ms);
                    }
                }
                _ => {}
            }
        }
        if !self.skipping && self.buffer.len() > MAX_LINE_BYTES {
            // Unterminated line already over the cap: drop the pending
            // event and skip the rest of the oversized line.
            self.drop_pending_event();
            self.skipping = true;
            self.buffer.clear();
        }
        events
    }

    /// Flush a trailing event at stream end (unterminated final block).
    pub fn finish(&mut self) -> Option<SseEvent> {
        let buffered = std::mem::take(&mut self.buffer);
        if self.skipping || buffered.len() > MAX_LINE_BYTES {
            self.skipping = false;
            self.drop_pending_event();
            return None;
        }
        let line = String::from_utf8_lossy(&buffered);
        let line = line.trim_end_matches('\r');
        if !line.is_empty() {
            if let Some((field, value)) = line.split_once(':') {
                if field == "data" {
                    self.data
                        .push(value.strip_prefix(' ').unwrap_or(value).to_string());
                }
            } else {
                self.data.push(line.to_string());
            }
        }
        self.dispatch()
    }

    /// Discard the half-built event (oversized line guard).
    fn drop_pending_event(&mut self) {
        self.data.clear();
        self.event_type = None;
        self.id = None;
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            self.event_type = None;
            return None;
        }
        let event = SseEvent {
            event: self.event_type.take(),
            data: self.data.join("\n"),
            id: self.id.take(),
        };
        self.data.clear();
        Some(event)
    }
}

/// Resolved HTTP configuration for one server (headers interpolated, command
/// secrets resolved at connection time — server-manager.ts:721-748).
pub struct HttpConfig {
    pub url: url::Url,
    pub headers: Vec<(String, String)>,
}

pub fn resolve_http_config(definition: &ServerEntry) -> Result<HttpConfig, ProtocolError> {
    resolve_http_config_with_server(definition, "unknown")
}

/// `server_name` feeds the `!command` error context strings (upstream
/// `MCP server "<name>" HTTP header "<key>"`).
pub fn resolve_http_config_with_server(
    definition: &ServerEntry,
    server_name: &str,
) -> Result<HttpConfig, ProtocolError> {
    let url = crate::utils::resolve_server_url(definition.get("url"))
        .map_err(|e| ProtocolError::Transport(e.to_string()))?
        .ok_or_else(|| ProtocolError::Transport("missing url".to_string()))?;
    let url =
        url::Url::parse(&url).map_err(|e| ProtocolError::Transport(format!("url parse: {e}")))?;
    let headers_map =
        crate::utils::resolve_command_secrets_record(definition.get("headers"), &|key| {
            format!("MCP server \"{server_name}\" HTTP header \"{key}\"")
        })
        .map_err(|e| ProtocolError::Transport(e.to_string()))?
        .unwrap_or_default();
    let mut headers: Vec<(String, String)> = headers_map
        .into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect();

    // Bearer injection (server-manager.ts:730-738, FR-P1-03): only
    // `auth: "bearer"` triggers it; a `!command` bearerToken is executed at
    // connection time; otherwise `resolveBearerToken` (bearerToken
    // interpolated, bearerTokenEnv read raw). The resolved token MUST NOT
    // reach logs (G4).
    if definition.get_str("auth") == Some("bearer") {
        let command_bearer = definition
            .get_str("bearerToken")
            .filter(|t| t.starts_with('!') && !t.starts_with("!!"));
        let token = match command_bearer {
            Some(command) => Some(
                crate::utils::resolve_command_secret(
                    command,
                    &format!("MCP server \"{server_name}\" HTTP bearer token"),
                )
                .map_err(|e| ProtocolError::Transport(e.to_string()))?,
            ),
            None => crate::utils::resolve_bearer_token(definition.as_map())
                .map_err(|e| ProtocolError::Transport(e.to_string()))?,
        };
        if let Some(token) = token {
            let authorization = format!("Bearer {token}");
            if let Some(existing) = headers
                .iter_mut()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            {
                existing.1 = authorization;
            } else {
                headers.push(("Authorization".to_string(), authorization));
            }
        }
    }
    Ok(HttpConfig { url, headers })
}

fn apply_headers(
    builder: reqwest::RequestBuilder,
    config: &HttpConfig,
    extra: &[(String, String)],
) -> reqwest::RequestBuilder {
    let mut builder = builder;
    for (name, value) in config.headers.iter().chain(extra.iter()) {
        builder = builder.header(name, value);
    }
    builder
}

/// The streamable HTTP transport (SDK 2.0 `StreamableHTTPClientTransport`,
/// P0 cut).
pub struct StreamableHttpTransport {
    client: reqwest::Client,
    config: HttpConfig,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    incoming: mpsc::UnboundedSender<Value>,
    closed: AtomicBool,
    /// Cancels all in-flight SSE streams on `close` (SDK `close()` aborts
    /// its `_abortController`). Parity note: no session-terminating DELETE
    /// is sent — the SDK exposes that as a separate `terminateSession()`
    /// method which server-manager.ts never calls.
    cancel: CancellationToken,
    /// Weak self-reference so `send` can spawn the standalone GET stream
    /// task after `notifications/initialized`.
    me: Mutex<std::sync::Weak<Self>>,
    /// Last event id seen on a GET stream (SDK `lastEventId`) — replayed as
    /// the `Last-Event-ID` header on reconnection.
    last_event_id: Mutex<Option<String>>,
    /// Server-provided reconnection delay (SSE `retry:` field, SDK
    /// `_serverRetryMs`); backoff applies while unset.
    server_retry_ms: Mutex<Option<u64>>,
    /// Requests whose per-request SSE stream closed without a response (SDK
    /// `replayMessageId`): one slot per pending request. The SDK assumes a
    /// single in-flight request (one `replayMessageId` slot); this client
    /// allows concurrent in-flight requests, so the slots form a set.
    replay_request_ids: Mutex<HashSet<u64>>,
}

/// SDK `DEFAULT_STREAMABLE_HTTP_RECONNECTION_OPTIONS`.
const RECONNECTION_INITIAL_DELAY: Duration = Duration::from_millis(1000);
const RECONNECTION_GROW_FACTOR: f64 = 2.0;
const RECONNECTION_MAX_DELAY: Duration = Duration::from_secs(30);
const RECONNECTION_MAX_RETRIES: u32 = 5;

impl StreamableHttpTransport {
    pub fn new(config: HttpConfig) -> (Arc<Self>, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let transport = Arc::new(Self {
            client: reqwest::Client::new(),
            config,
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            incoming: tx,
            closed: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            me: Mutex::new(std::sync::Weak::new()),
            last_event_id: Mutex::new(None),
            server_retry_ms: Mutex::new(None),
            replay_request_ids: Mutex::new(HashSet::new()),
        });
        *transport.me.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&transport);
        (transport, rx)
    }

    fn extra_headers(&self, is_handshake: bool) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        if !is_handshake {
            if let Some(session) = self
                .session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                headers.push(("mcp-session-id".to_string(), session));
            }
        }
        if let Some(version) = self
            .protocol_version
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            headers.push(("mcp-protocol-version".to_string(), version));
        }
        headers
    }

    /// The standalone GET SSE stream (SDK `_startOrAuthSse`): opened after
    /// `notifications/initialized`; 405 means the server has no stream.
    fn open_standalone_stream(self: &Arc<Self>) {
        self.start_standalone_stream(0);
    }

    fn start_standalone_stream(self: &Arc<Self>, attempt: u32) {
        let this = self.clone();
        tokio::spawn(async move {
            if attempt >= RECONNECTION_MAX_RETRIES {
                debug!("MCP streamable: max reconnection attempts exceeded");
                return;
            }
            let extra = this.extra_headers(false);
            let mut request = apply_headers(
                this.client.get(this.config.url.clone()),
                &this.config,
                &extra,
            )
            .header("accept", "text/event-stream");
            // SDK resumability: replay the last seen event id.
            if let Some(last_event_id) = this
                .last_event_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                request = request.header("last-event-id", last_event_id);
            }
            let response = tokio::select! {
                _ = this.cancel.cancelled() => return,
                sent = request.send() => match sent {
                    Ok(response) => response,
                    Err(error) => {
                        debug!(%error, "MCP streamable GET stream failed to open");
                        this.schedule_standalone_reconnect(attempt).await;
                        return;
                    }
                },
            };
            if !response.status().is_success() {
                // 405: no standalone stream (SDK treats it as end-of-stream).
                return;
            }
            this.pump_sse(response).await;
            // A stream that ended after delivering events reconnects with
            // the attempt counter reset (SDK `_handleSseStream` re-enters
            // `_scheduleReconnection` at attemptCount 0; only GET failures
            // count toward the retry cap).
            this.schedule_standalone_reconnect(0).await;
        });
    }

    /// `_scheduleReconnection` (SDK 5172-5197): server-provided `retry:`
    /// wins; otherwise exponential backoff from 1s (grow 2×, cap 30s).
    async fn schedule_standalone_reconnect(self: &Arc<Self>, attempt: u32) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let delay = self.reconnection_delay(attempt);
        tokio::select! {
            _ = self.cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        self.start_standalone_stream(attempt + 1);
    }

    fn reconnection_delay(&self, attempt: u32) -> Duration {
        match *self
            .server_retry_ms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            Some(ms) => Duration::from_millis(ms),
            None => {
                let raw = RECONNECTION_INITIAL_DELAY.as_millis() as f64
                    * RECONNECTION_GROW_FACTOR.powi(attempt as i32);
                Duration::from_millis(raw.min(RECONNECTION_MAX_DELAY.as_millis() as f64) as u64)
            }
        }
    }

    /// Per-request stream replay (SDK `_handleSseStream` →
    /// `_scheduleReconnection` with `replayMessageId`): reopen the GET
    /// stream after the reconnection delay. Responses on it carry their
    /// original request ids — the client's pending table dispatches by id,
    /// so they are forwarded unchanged. (The SDK instead overwrites
    /// `message.id = replayMessageId` because it tracks one in-flight
    /// request; with concurrent in-flight requests that remap would deliver
    /// responses to the wrong request.)
    async fn schedule_request_replay(self: &Arc<Self>, request_id: u64, mut attempt: u32) {
        loop {
            if self.closed.load(Ordering::SeqCst) || attempt >= RECONNECTION_MAX_RETRIES {
                return;
            }
            let delay = self.reconnection_delay(attempt);
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            if !self
                .replay_request_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&request_id)
            {
                return; // resolved meanwhile
            }
            let extra = self.extra_headers(false);
            let mut request = apply_headers(
                self.client.get(self.config.url.clone()),
                &self.config,
                &extra,
            )
            .header("accept", "text/event-stream");
            if let Some(last_event_id) = self
                .last_event_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                request = request.header("last-event-id", last_event_id);
            }
            let response = tokio::select! {
                _ = self.cancel.cancelled() => return,
                sent = request.send() => match sent {
                    Ok(response) if response.status().is_success() => response,
                    Ok(response) => {
                        debug!(status = %response.status(), "MCP streamable replay GET rejected");
                        attempt += 1;
                        continue;
                    }
                    Err(error) => {
                        debug!(%error, "MCP streamable replay GET failed");
                        attempt += 1;
                        continue;
                    }
                },
            };
            // Events are forwarded unchanged (ids ride the payload).
            self.pump_sse(response).await;
            // Still unresolved: retry (SDK attempts loop; the counter
            // starts over after a successfully opened stream, matching the
            // SDK's `_scheduleReconnection(options, 0)` on stream end).
            if !self
                .replay_request_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&request_id)
            {
                return;
            }
            attempt = 0;
        }
    }

    /// Read an SSE response body, forwarding every `message` (or untyped)
    /// event's JSON payload to the client channel (SDK `_handleSseStream`).
    /// Returns when the stream ends or the transport is closed.
    async fn pump_sse(&self, response: reqwest::Response) {
        let mut decoder = SseDecoder::new();
        let mut stream = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                _ = self.cancel.cancelled() => break,
                chunk = stream.next() => match chunk {
                    Some(Ok(chunk)) => chunk,
                    _ => break,
                },
            };
            for event in decoder.feed(&chunk) {
                self.forward_sse_event(event);
            }
        }
        if let Some(event) = decoder.finish() {
            self.forward_sse_event(event);
        }
        if let Some(retry_ms) = decoder.retry_ms {
            *self
                .server_retry_ms
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(retry_ms);
        }
    }

    fn forward_sse_event(&self, event: SseEvent) {
        if let Some(id) = &event.id {
            if !id.is_empty() {
                *self.last_event_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
            }
        }
        if event.event.is_some() && event.event.as_deref() != Some("message") {
            return;
        }
        match serde_json::from_str::<Value>(&event.data) {
            Ok(message) => {
                // A response clears its request's replay wait (SDK
                // `receivedResponse`); the message id is forwarded as-is.
                if message.get("result").is_some() || message.get("error").is_some() {
                    if let Some(id) = message.get("id").and_then(Value::as_u64) {
                        self.replay_request_ids
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&id);
                    }
                }
                let _ = self.incoming.send(message);
            }
            Err(error) => debug!(%error, "MCP streamable: dropping non-JSON SSE event"),
        }
    }
}

#[async_trait::async_trait]
impl McpTransport for StreamableHttpTransport {
    async fn send(&self, message: Value) -> Result<(), ProtocolError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ProtocolError::Closed);
        }
        let is_handshake = message.get("method").and_then(Value::as_str) == Some("initialize");
        let is_initialized_notification =
            message.get("method").and_then(Value::as_str) == Some("notifications/initialized");
        let has_request = message.get("id").is_some() && message.get("method").is_some();

        let mut builder = self.client.post(self.config.url.clone());
        builder = apply_headers(builder, &self.config, &self.extra_headers(is_handshake));
        builder = builder
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        let response = builder
            .body(serde_json::to_string(&message).unwrap_or_else(|_| "null".to_string()))
            .send()
            .await
            .map_err(|e| ProtocolError::Transport(format!("http send: {e}")))?;

        if is_handshake && response.status().is_success() {
            let session = response
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            *self.session_id.lock().unwrap_or_else(|e| e.into_inner()) = session;
        }

        let status = response.status();
        if status.as_u16() == 401 {
            return Err(ProtocolError::Unauthorized);
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProtocolError::Http {
                status: status.as_u16(),
                // The body can carry endpoint details; it is NOT credential
                // material (credentials ride our request, not the response).
                message: format!("Error POSTing to endpoint: {text}"),
            });
        }
        if status.as_u16() == 202 {
            if is_initialized_notification {
                if let Some(this) = self.me.lock().unwrap_or_else(|e| e.into_inner()).upgrade() {
                    this.open_standalone_stream();
                }
            }
            return Ok(());
        }
        if has_request {
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .unwrap_or_default();
            let media = content_type.split(';').next().unwrap_or("").trim();
            if media == "text/event-stream" {
                // SDK `send()` runs `_handleSseStream` without awaiting it
                // and returns at the headers: the response rides the
                // incoming channel, and a server that keeps the stream
                // open after delivering it must not hold `send` (and thus
                // the caller's timeout race) hostage.
                let request_id = message.get("id").and_then(Value::as_u64);
                if let Some(this) = self.me.lock().unwrap_or_else(|e| e.into_inner()).upgrade() {
                    tokio::spawn(async move {
                        this.pump_sse(response).await;
                        // SDK `_handleSseStream`: a per-request stream that
                        // closed without delivering the response schedules
                        // a reconnection GET; a replayed response satisfies
                        // the pending id. Registering the slot here (after
                        // the pump) — insert() returns false if the
                        // response already resolved this id.
                        if let Some(id) = request_id {
                            if this
                                .replay_request_ids
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(id)
                            {
                                this.schedule_request_replay(id, 0).await;
                            }
                        }
                    });
                }
                // No strong reference left: dropping `response` closes the
                // stream (nobody is listening for events).
            } else if media == "application/json" {
                let body = response
                    .text()
                    .await
                    .map_err(|e| ProtocolError::Transport(format!("read body: {e}")))?;
                let data: Value = serde_json::from_str(&body)
                    .map_err(|e| ProtocolError::Protocol(format!("bad JSON response: {e}")))?;
                let messages = data.as_array().cloned().unwrap_or_else(|| vec![data]);
                for message in messages {
                    let _ = self.incoming.send(message);
                }
            } else {
                return Err(ProtocolError::Protocol(format!(
                    "Unexpected content type: {content_type}"
                )));
            }
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), ProtocolError> {
        self.closed.store(true, Ordering::SeqCst);
        // Abort the in-flight SSE streams (SDK `close()` aborts its
        // `_abortController`; no DELETE — see the `cancel` field docs).
        self.cancel.cancel();
        Ok(())
    }

    fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_protocol_version(&self, version: &str) {
        *self
            .protocol_version
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(version.to_string());
    }
}

/// The legacy SSE transport (SDK 2.0 `SSEClientTransport`, P0 cut).
pub struct LegacySseTransport {
    client: reqwest::Client,
    config: HttpConfig,
    endpoint: Mutex<Option<url::Url>>,
    incoming: mpsc::UnboundedSender<Value>,
    closed: AtomicBool,
}

impl LegacySseTransport {
    /// Open the GET stream and wait for the `endpoint` event (SDK
    /// `_startOrAuth` / the `endpoint` listener with the origin check).
    pub async fn connect(
        config: HttpConfig,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Value>), ProtocolError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let origin = config.url.origin().ascii_serialization();
        let transport = Arc::new(Self {
            client: reqwest::Client::new(),
            config,
            endpoint: Mutex::new(None),
            incoming: tx,
            closed: AtomicBool::new(false),
        });

        let request = apply_headers(
            transport.client.get(transport.config.url.clone()),
            &transport.config,
            &[],
        )
        .header("accept", "text/event-stream");
        let response = request
            .send()
            .await
            .map_err(|e| ProtocolError::Transport(format!("sse connect: {e}")))?;
        if response.status().as_u16() == 401 {
            return Err(ProtocolError::Unauthorized);
        }
        if !response.status().is_success() {
            return Err(ProtocolError::Http {
                status: response.status().as_u16(),
                message: format!(
                    "Failed to open SSE stream: {}",
                    response.status().canonical_reason().unwrap_or("")
                ),
            });
        }

        // Read the stream on a task; the connect future resolves once the
        // `endpoint` event arrives.
        let (endpoint_tx, endpoint_rx) = tokio::sync::oneshot::channel();
        let this = transport.clone();
        tokio::spawn(async move {
            let mut decoder = SseDecoder::new();
            let mut endpoint_tx = Some(endpoint_tx);
            let mut stream = response.bytes_stream();
            loop {
                let chunk = match stream.next().await {
                    Some(Ok(chunk)) => chunk,
                    _ => {
                        // Stream ended: flush a trailing event, then stop.
                        if let Some(event) = decoder.finish() {
                            this.handle_sse_event(event, &origin, &mut endpoint_tx);
                        }
                        break;
                    }
                };
                for event in decoder.feed(&chunk) {
                    this.handle_sse_event(event, &origin, &mut endpoint_tx);
                }
            }
        });

        let endpoint = endpoint_rx
            .await
            .map_err(|_| ProtocolError::Closed)?
            .map_err(|e: ProtocolError| e)?;
        *transport.endpoint.lock().unwrap_or_else(|e| e.into_inner()) = Some(endpoint);
        Ok((transport, rx))
    }

    fn handle_sse_event(
        &self,
        event: SseEvent,
        origin: &str,
        endpoint_tx: &mut Option<tokio::sync::oneshot::Sender<Result<url::Url, ProtocolError>>>,
    ) {
        match event.event.as_deref() {
            Some("endpoint") => {
                let resolved = self.config.url.join(&event.data);
                let outcome = match resolved {
                    Ok(url) if url.origin().ascii_serialization() == origin => Ok(url),
                    Ok(url) => Err(ProtocolError::Protocol(format!(
                        "Endpoint origin does not match connection origin: {}",
                        url.origin().ascii_serialization()
                    ))),
                    Err(error) => Err(ProtocolError::Protocol(format!("bad endpoint: {error}"))),
                };
                if let Some(tx) = endpoint_tx.take() {
                    let _ = tx.send(outcome);
                }
            }
            // Untyped and `message` events carry JSON-RPC payloads.
            _ => match serde_json::from_str::<Value>(&event.data) {
                Ok(message) => {
                    let _ = self.incoming.send(message);
                }
                Err(error) => debug!(%error, "MCP sse: dropping non-JSON event"),
            },
        }
    }
}

#[async_trait::async_trait]
impl McpTransport for LegacySseTransport {
    async fn send(&self, message: Value) -> Result<(), ProtocolError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ProtocolError::Closed);
        }
        let endpoint = self
            .endpoint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(ProtocolError::Closed)?;
        let builder = apply_headers(self.client.post(endpoint), &self.config, &[])
            .header("content-type", "application/json");
        let response = builder
            .body(serde_json::to_string(&message).unwrap_or_else(|_| "null".to_string()))
            .send()
            .await
            .map_err(|e| ProtocolError::Transport(format!("sse send: {e}")))?;
        if response.status().as_u16() == 401 {
            return Err(ProtocolError::Unauthorized);
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ProtocolError::Http {
                status,
                message: format!("Error POSTing to endpoint (HTTP {status}): {text}"),
            });
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), ProtocolError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn sse_decoder_aggregates_data_lines_and_dispatches_on_blank() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(b"event: message\ndata: {\"a\":").is_empty());
        let events = decoder.feed(b"1}\ndata: {\"b\":2}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "{\"a\":1}\n{\"b\":2}");
    }

    #[test]
    fn sse_decoder_skips_comments_and_flushes_tail() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(b": ping\n\ndata: tail").is_empty());
        let tail = decoder.finish().expect("tail event");
        assert_eq!(tail.data, "tail");
        assert_eq!(tail.event, None);
    }

    #[test]
    fn sse_decoder_handles_crlf() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"event: endpoint\r\ndata: /msg\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("endpoint"));
        assert_eq!(events[0].data, "/msg");
    }

    #[test]
    fn sse_decoder_survives_utf8_split_across_chunks() {
        // Multibyte code points split at arbitrary byte offsets across two
        // feeds must decode intact (no U+FFFD). "你好" + "🎉" = 3+3+4 bytes.
        let payload = "data: {\"t\": \"你好🎉\"}\n\n";
        let bytes = payload.as_bytes();
        for split in 0..bytes.len() {
            let mut decoder = SseDecoder::new();
            assert!(
                decoder.feed(&bytes[..split]).is_empty(),
                "no event before the blank line (split at {})",
                split
            );
            let events = decoder.feed(&bytes[split..]);
            assert_eq!(events.len(), 1, "split at {split}");
            assert_eq!(events[0].data, "{\"t\": \"你好🎉\"}");
        }
    }

    #[test]
    fn sse_decoder_delivers_large_legitimate_data_lines() {
        // Regression (tavily tools/list timeout): a real server's tools/list
        // or tool result routinely puts tens/hundreds of KiB on ONE data
        // line. The cap guards memory (10 MiB, SDK ReadBuffer maxBufferSize)
        // — it must not eat legitimate payloads.
        let big = "y".repeat(512 * 1024);
        let frame = format!("event: message\r\ndata: {{\"r\":\"{big}\"}}\r\n\r\n");
        let mut decoder = SseDecoder::new();
        // Split across two feeds to exercise the accumulation path.
        let bytes = frame.into_bytes();
        let mid = bytes.len() / 2;
        assert!(decoder.feed(&bytes[..mid]).is_empty());
        let events = decoder.feed(&bytes[mid..]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, format!("{{\"r\":\"{big}\"}}"));
    }

    #[test]
    fn sse_decoder_drops_events_with_oversized_lines() {
        let mut decoder = SseDecoder::new();
        let long = "x".repeat(MAX_LINE_BYTES + 1);
        let mut frame = format!("event: message\ndata: {long}\n\n").into_bytes();
        // Same event continued past the cap: dropped entirely, then a valid
        // event after it decodes normally.
        frame.extend_from_slice(b"data: ok\n\n");
        let events = decoder.feed(&frame);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "ok");
        assert_eq!(events[0].event, None);

        // An unterminated oversized line also can't grow the buffer.
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(&vec![b'a'; 4 * MAX_LINE_BYTES]).is_empty());
        assert!(decoder.buffer.capacity() <= 4 * MAX_LINE_BYTES);
        let events = decoder.feed(b"\n\ndata: after\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "after");
    }

    /// B-2 regression: the MCP spec lets a server keep the per-request SSE
    /// stream open after delivering the response. `send` must return at the
    /// response headers (the SDK does not await `_handleSseStream`), with
    /// the response still delivered through the incoming channel.
    #[tokio::test]
    async fn send_returns_before_sse_stream_closes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let body = loop {
                let n = socket.read(&mut tmp).await.expect("read request");
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let content_length: usize = head
                        .lines()
                        .find_map(|l| l.split_once(':'))
                        .and_then(|(k, v)| {
                            (k.trim().eq_ignore_ascii_case("content-length"))
                                .then(|| v.trim().parse().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    let body_start = pos + 4;
                    if buf.len() >= body_start + content_length {
                        break String::from_utf8_lossy(&buf[body_start..]).to_string();
                    }
                }
            };
            let message: Value = serde_json::from_str(&body).expect("request JSON");
            // 200 + SSE content type, one response event, then the stream
            // stays open until the client goes away.
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
            socket.write_all(head.as_bytes()).await.expect("head");
            let event = format!(
                "data: {}\n\n",
                json!({ "jsonrpc": "2.0", "id": message["id"], "result": {} })
            );
            socket.write_all(event.as_bytes()).await.expect("event");
            let mut sink = [0u8; 64];
            // Park until the transport closes the connection.
            let _ = socket.read(&mut sink).await;
        });

        let config = HttpConfig {
            url: url::Url::parse(&format!("http://{addr}/mcp")).expect("url"),
            headers: Vec::new(),
        };
        let (transport, mut rx) = StreamableHttpTransport::new(config);
        // The server never closes the stream, so a `send` that awaits the
        // SSE body would hang until this timeout fires.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            transport.send(json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" })),
        )
        .await
        .expect("send must return while the SSE stream stays open")
        .expect("send ok");
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("response arrives on the incoming channel")
            .expect("channel open");
        assert_eq!(message["id"], json!(7));
        assert!(message.get("result").is_some());

        transport.close().await.expect("close");
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("server task ends once close cancels the stream")
            .expect("server join");
    }
}
