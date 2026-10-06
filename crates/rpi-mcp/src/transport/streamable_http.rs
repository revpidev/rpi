//! streamable HTTP transport (port of
//! `packages/mcp/src/transports/streamable-http.ts` @ a13d35a74).
//!
//! POST carries requests/notifications; responses arrive as JSON or as an
//! SSE stream on the POST response, and the server may additionally expose a
//! GET SSE stream for server-to-client messages. 401 (or 403 asking for
//! more scope) is handed to the [`AuthProvider`] once. SSE streams resume
//! with `Last-Event-ID` and reconnect with exponential backoff.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{DEFAULT_MAX_MESSAGE_BYTES, McpTransport, McpTransportError, TransportEvents};
use crate::auth_provider::{AuthProvider, UnauthorizedContext};
use crate::protocol::{
    JSON_RPC_ERROR_INTERNAL, JsonRpcId, JsonRpcMessage, is_json_rpc_request, parse_json_rpc_id,
    parse_json_rpc_message,
};

/// `MAX_ERROR_BODY_BYTES` (streamable-http.ts:16).
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
/// `ERROR_MESSAGE_BODY_CHARS` (streamable-http.ts:17).
const ERROR_MESSAGE_BODY_CHARS: usize = 500;
/// `DEFAULT_RECONNECT_INITIAL_DELAY_MS` (streamable-http.ts:18).
const DEFAULT_RECONNECT_INITIAL_DELAY_MS: u64 = 1_000;
/// `DEFAULT_RECONNECT_MAX_DELAY_MS` (streamable-http.ts:19).
const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 30_000;
/// `DEFAULT_RECONNECT_MAX_RETRIES` (streamable-http.ts:20).
const DEFAULT_RECONNECT_MAX_RETRIES: u32 = 5;

/// `StreamableHttpReconnectOptions` (streamable-http.ts:83).
#[derive(Debug, Clone)]
pub struct StreamableHttpReconnectOptions {
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub max_retries: u32,
}

impl Default for StreamableHttpReconnectOptions {
    fn default() -> Self {
        Self {
            initial_delay_ms: DEFAULT_RECONNECT_INITIAL_DELAY_MS,
            max_delay_ms: DEFAULT_RECONNECT_MAX_DELAY_MS,
            max_retries: DEFAULT_RECONNECT_MAX_RETRIES,
        }
    }
}

/// `StreamableHttpTransportOptions` (streamable-http.ts:94).
#[derive(Clone)]
pub struct StreamableHttpTransportOptions {
    pub url: String,
    pub headers: HashMap<String, String>,
    /// Injectable HTTP client (tests); the default client otherwise.
    pub client: Option<reqwest::Client>,
    /// Open the server-to-client GET stream after initialization
    /// (default: true).
    pub open_get_stream: bool,
    pub max_message_bytes: Option<usize>,
    pub auth_provider: Option<Arc<dyn AuthProvider>>,
    pub reconnect: StreamableHttpReconnectOptions,
}

impl StreamableHttpTransportOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: HashMap::new(),
            client: None,
            open_get_stream: true,
            max_message_bytes: None,
            auth_provider: None,
            reconnect: StreamableHttpReconnectOptions::default(),
        }
    }
}

/// `SseEvent` (streamable-http.ts:22).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// `ConsumeSseOptions` (streamable-http.ts:28).
pub struct ConsumeSseOptions<'a> {
    pub max_event_bytes: usize,
    pub on_event: &'a mut (dyn FnMut(SseEvent) + Send),
    pub on_id: &'a mut (dyn FnMut(&str) + Send),
    pub on_retry: &'a mut (dyn FnMut(u64) + Send),
}

/// `consumeSseStream` (streamable-http.ts:39): incremental SSE decoding with
/// per-event size accounting so half-open events cannot grow unbounded.
pub async fn consume_sse_stream<S>(
    stream: S,
    options: &mut ConsumeSseOptions<'_>,
) -> Result<(), McpTransportError>
where
    S: Stream<Item = Result<Bytes, McpTransportError>> + Unpin,
{
    fn dispatch(
        event_name: &mut Option<String>,
        event_id: &mut Option<String>,
        data_lines: &mut Vec<String>,
        data_bytes: &mut usize,
        options: &mut ConsumeSseOptions<'_>,
    ) {
        if data_lines.is_empty() {
            *event_name = None;
            *event_id = None;
            return;
        }
        let data = data_lines.join("\n");
        (options.on_event)(SseEvent {
            event: event_name.take(),
            data,
            id: event_id.take(),
        });
        data_lines.clear();
        *data_bytes = 0;
    }

    fn process_line(
        raw_line: &[u8],
        event_name: &mut Option<String>,
        event_id: &mut Option<String>,
        data_lines: &mut Vec<String>,
        data_bytes: &mut usize,
        options: &mut ConsumeSseOptions<'_>,
    ) -> Result<(), McpTransportError> {
        let raw_line = String::from_utf8_lossy(raw_line);
        let line = raw_line.strip_suffix('\r').unwrap_or(&raw_line);
        if line.is_empty() {
            dispatch(event_name, event_id, data_lines, data_bytes, options);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.find(':') {
            Some(colon) => (&line[..colon], &line[colon + 1..]),
            None => (line, ""),
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                *data_bytes += value.len() + usize::from(!data_lines.is_empty());
                if *data_bytes > options.max_event_bytes {
                    return Err(McpTransportError::Other(format!(
                        "MCP SSE event exceeds {} bytes",
                        options.max_event_bytes
                    )));
                }
                data_lines.push(value.to_owned());
            }
            "event" => *event_name = Some(value.to_owned()),
            "id" => {
                if !value.contains('\0') {
                    *event_id = Some(value.to_owned());
                    (options.on_id)(value);
                }
            }
            "retry" => {
                if !value.is_empty()
                    && value.bytes().all(|byte| byte.is_ascii_digit())
                    && let Ok(delay) = value.parse::<u64>()
                {
                    (options.on_retry)(delay);
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut stream = stream;
    let mut buffered: Vec<u8> = Vec::new();
    let mut at_stream_start = true;
    let mut event_name: Option<String> = None;
    let mut event_id: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();
    let mut data_bytes = 0usize;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffered.extend_from_slice(&chunk);
        if at_stream_start && buffered.len() >= 3 {
            // A leading UTF-8 BOM would otherwise turn the first `data`
            // field into `\u{FEFF}data` and drop the event (v0.1.6 review
            // P3). The check waits for three bytes so a split BOM still
            // matches.
            if buffered.starts_with(&[0xEF, 0xBB, 0xBF]) {
                buffered.drain(..3);
            }
            at_stream_start = false;
        }
        while let Some(newline) = buffered.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffered.drain(..=newline).collect();
            process_line(
                &line[..line.len() - 1],
                &mut event_name,
                &mut event_id,
                &mut data_lines,
                &mut data_bytes,
                options,
            )?;
        }
        if buffered.len() > options.max_event_bytes {
            return Err(McpTransportError::Other(format!(
                "MCP SSE event exceeds {} bytes",
                options.max_event_bytes
            )));
        }
    }
    if !buffered.is_empty() {
        process_line(
            &buffered,
            &mut event_name,
            &mut event_id,
            &mut data_lines,
            &mut data_bytes,
            options,
        )?;
    }
    dispatch(
        &mut event_name,
        &mut event_id,
        &mut data_lines,
        &mut data_bytes,
        options,
    );
    Ok(())
}

type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, McpTransportError>> + Send>>;

#[derive(Default)]
struct StreamCursor {
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
    /// Whether the stream delivered any event since it was (re)opened.
    received: bool,
}

fn content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
}

/// `needsAuthorization` (streamable-http.ts:168): 401, or 403 with an
/// `insufficient_scope` bearer challenge (step-up authorization).
fn needs_authorization(response: &reqwest::Response) -> bool {
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return true;
    }
    if response.status() != reqwest::StatusCode::FORBIDDEN {
        return false;
    }
    www_authenticate(response)
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("insufficient_scope")
}

fn www_authenticate(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// `isTransientStatus` (streamable-http.ts:173): statuses worth retrying
/// when a stream fails to (re)open.
fn is_transient_status(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

/// `describeHttpFailure` (streamable-http.ts:185).
fn describe_http_failure(status: u16, body: &str) -> String {
    let text = body.trim();
    let snippet = if text.chars().count() > ERROR_MESSAGE_BODY_CHARS {
        let prefix: String = text.chars().take(ERROR_MESSAGE_BODY_CHARS - 3).collect();
        format!("{prefix}...")
    } else {
        text.to_owned()
    };
    if snippet.is_empty() {
        format!("MCP HTTP request failed with status {status}")
    } else {
        format!("MCP HTTP request failed with status {status}: {snippet}")
    }
}

fn http_error(
    status: u16,
    message: impl Into<String>,
    body: impl Into<String>,
) -> McpTransportError {
    McpTransportError::http(status, message, body)
}

/// `isRetryable` (streamable-http.ts:495): network failures and transient
/// statuses are retried; auth, session, and protocol errors are not.
fn is_retryable(error: &McpTransportError) -> bool {
    match error {
        McpTransportError::Http { status, .. } => is_transient_status(*status),
        McpTransportError::Io(_) => true,
        _ => false,
    }
}

fn map_reqwest_error(error: reqwest::Error) -> McpTransportError {
    McpTransportError::Io(error.to_string())
}

/// Box a response body as a byte stream of transport errors.
fn body_stream(response: reqwest::Response) -> BodyStream {
    Box::pin(
        response
            .bytes_stream()
            .map(|chunk| chunk.map_err(map_reqwest_error)),
    )
}

/// State shared between the transport and the detached stream readers.
struct HttpShared {
    url: Url,
    http: reqwest::Client,
    options: StreamableHttpTransportOptions,
    controller: CancellationToken,
    closed: AtomicBool,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    events: Arc<TransportEvents>,
}

impl HttpShared {
    async fn headers(
        &self,
        extra: &[(String, String)],
    ) -> Result<(reqwest::header::HeaderMap, Option<String>), McpTransportError> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &self.options.headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        for (name, value) in extra {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        if let Some(session) = lock(&self.session_id).clone()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&session)
        {
            headers.insert("Mcp-Session-Id", value);
        }
        if let Some(version) = lock(&self.protocol_version).clone()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&version)
        {
            headers.insert("MCP-Protocol-Version", value);
        }
        let mut token = None;
        if let Some(auth) = &self.options.auth_provider
            && let Some(value) = auth.token().await
            && let Ok(header) = reqwest::header::HeaderValue::from_str(&format!("Bearer {value}"))
        {
            headers.insert(reqwest::header::AUTHORIZATION, header);
            token = Some(value);
        }
        Ok((headers, token))
    }

    /// `authorizedFetch` (streamable-http.ts:307): a 401 (or 403 asking for
    /// more scope) is handed to the auth provider once and the request is
    /// retried with the credentials it left behind.
    async fn authorized_fetch(
        &self,
        method: reqwest::Method,
        extra: Vec<(String, String)>,
        body: Option<String>,
    ) -> Result<reqwest::Response, McpTransportError> {
        let mut attempt = 0;
        loop {
            let (headers, token) = self.headers(&extra).await?;
            let mut request = self
                .http
                .request(method.clone(), self.url.clone())
                .headers(headers);
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            let response = request.send().await.map_err(map_reqwest_error)?;
            let provider = self.options.auth_provider.clone();
            if attempt > 0 || provider.is_none() || !needs_authorization(&response) {
                return Ok(response);
            }
            let provider = provider.expect("checked");
            let context = UnauthorizedContext {
                status: response.status().as_u16(),
                www_authenticate: www_authenticate(&response),
                server_url: self.url.clone(),
                token,
            };
            attempt += 1;
            let result = provider.on_unauthorized(context).await;
            drop(response);
            match result {
                Ok(()) => {}
                Err(crate::protocol::McpError::AuthorizationRequired) => {
                    return Err(McpTransportError::AuthorizationRequired);
                }
                Err(error) => return Err(McpTransportError::Other(error.to_string())),
            }
        }
    }

    /// `checkResponse` (streamable-http.ts:354).
    async fn check_response(
        &self,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, McpTransportError> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response
            .bytes()
            .await
            .map(|bytes| {
                let bytes = bytes.slice(..bytes.len().min(MAX_ERROR_BODY_BYTES));
                String::from_utf8_lossy(&bytes).into_owned()
            })
            .unwrap_or_default();
        if status == 401 {
            return Err(McpTransportError::AuthRequired {
                www_authenticate: headers
                    .get(reqwest::header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                body,
            });
        }
        if status == 404 && lock(&self.session_id).is_some() {
            return Err(McpTransportError::SessionExpired { body });
        }
        Err(http_error(
            status,
            describe_http_failure(status, &body),
            body,
        ))
    }

    fn capture_session(&self, response: &reqwest::Response) {
        if let Some(value) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *lock(&self.session_id) = Some(value.to_owned());
        }
    }

    /// `reconnectDelay` (streamable-http.ts:503).
    fn reconnect_delay(&self, attempt: u32, server_delay_ms: Option<u64>) -> u64 {
        if let Some(server_delay) = server_delay_ms {
            return server_delay;
        }
        self.options
            .reconnect
            .initial_delay_ms
            .saturating_mul(2u64.saturating_pow(attempt))
            .min(self.options.reconnect.max_delay_ms)
    }

    /// `sleep` (streamable-http.ts:515): `false` when the transport closed
    /// while waiting.
    async fn sleep(&self, ms: u64) -> bool {
        tokio::select! {
            () = self.controller.cancelled() => false,
            () = tokio::time::sleep(Duration::from_millis(ms)) => true,
        }
    }

    /// `openSseStream` (streamable-http.ts:481). `None` when the server
    /// answers 405 (no GET stream).
    async fn open_sse_stream(
        &self,
        last_event_id: Option<&str>,
    ) -> Result<Option<BodyStream>, McpTransportError> {
        let mut extra = vec![("accept".to_owned(), "text/event-stream".to_owned())];
        if let Some(last_event_id) = last_event_id {
            extra.push(("last-event-id".to_owned(), last_event_id.to_owned()));
        }
        let response = self
            .authorized_fetch(reqwest::Method::GET, extra, None)
            .await?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Ok(None);
        }
        let response = self.check_response(response).await?;
        self.capture_session(&response);
        let content_type = content_type(&response);
        if content_type.as_deref() != Some("text/event-stream") {
            return Err(http_error(
                response.status().as_u16(),
                format!(
                    "Unsupported MCP GET response content type: {}",
                    content_type.unwrap_or_else(|| "missing".to_owned())
                ),
                String::new(),
            ));
        }
        Ok(Some(body_stream(response)))
    }

    /// `consumeSse` (streamable-http.ts:369): parse events, deliver messages
    /// through `on_message` first (the response-stream answered flag), then
    /// emit them on the shared bus.
    async fn consume_sse(
        &self,
        stream: BodyStream,
        cursor: &mut StreamCursor,
        mut on_message: Option<&mut (dyn FnMut(&JsonRpcMessage) + Send)>,
    ) -> Result<(), McpTransportError> {
        let max_event_bytes = self
            .options
            .max_message_bytes
            .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES);
        let mut parsed_events: Vec<JsonRpcMessage> = Vec::new();
        let mut parse_errors: Vec<McpTransportError> = Vec::new();
        {
            let last_event_id = &mut cursor.last_event_id;
            let retry_ms = &mut cursor.retry_ms;
            let received = &mut cursor.received;
            let mut options = ConsumeSseOptions {
                max_event_bytes,
                on_event: &mut |event| {
                    *received = true;
                    // Events without data prime resumption; other event types
                    // are not JSON-RPC.
                    if event.data.trim().is_empty()
                        || event.event.as_deref().is_some_and(|name| name != "message")
                    {
                        return;
                    }
                    match serde_json::from_str::<serde_json::Value>(&event.data)
                        .ok()
                        .and_then(parse_json_rpc_message)
                    {
                        Some(message) => parsed_events.push(message),
                        None => parse_errors.push(McpTransportError::Other(
                            "MCP SSE event is not a valid JSON-RPC message".to_owned(),
                        )),
                    }
                },
                on_id: &mut |id| *last_event_id = Some(id.to_owned()),
                on_retry: &mut |delay| *retry_ms = Some(delay),
            };
            consume_sse_stream(stream, &mut options).await?;
        }
        for error in parse_errors {
            self.events.emit_error(&error);
        }
        for message in parsed_events {
            if let Some(callback) = on_message.as_deref_mut() {
                callback(&message);
            }
            self.events.emit_message(&message);
        }
        Ok(())
    }

    /// `consumeResponseStream` (streamable-http.ts:407): read the SSE stream
    /// answering one request; when it ends or breaks before the response
    /// arrives and the server assigned event IDs, resume it with GET and
    /// `Last-Event-ID`. Otherwise only this request fails.
    async fn consume_response_stream(self: &Arc<Self>, first: BodyStream, request_id: JsonRpcId) {
        let mut cursor = StreamCursor::default();
        let mut answered = false;
        let mut failure: Option<McpTransportError> = None;
        let mut stream = Some(first);
        let mut attempt = 0u32;
        loop {
            if let Some(current) = stream.take() {
                let answered_flag = &mut answered;
                let mut on_message = |message: &JsonRpcMessage| {
                    if let JsonRpcMessage::Response { id, .. } = message
                        // Numeric identity (v0.1.6 review round 2, O2): the
                        // client id `1` and a server echo `1.0` are the same
                        // JavaScript number; exact `==` left the stream
                        // "unanswered", triggering a spurious GET resume and
                        // an "unknown request" error after the response had
                        // already been consumed.
                        && id.numerically_eq(&request_id)
                    {
                        *answered_flag = true;
                    }
                };
                let result = self
                    .consume_sse(current, &mut cursor, Some(&mut on_message))
                    .await;
                failure = result.err();
            }
            if answered || self.closed.load(Ordering::SeqCst) {
                return;
            }
            if let Some(error) = &failure
                && !is_retryable(error)
            {
                break;
            }
            let Some(last_event_id) = cursor.last_event_id.clone() else {
                break;
            };
            if attempt >= self.options.reconnect.max_retries {
                break;
            }
            if cursor.received {
                attempt = 0;
            }
            cursor.received = false;
            let delay = self.reconnect_delay(attempt, cursor.retry_ms);
            attempt += 1;
            if !self.sleep(delay).await {
                return;
            }
            match self.open_sse_stream(Some(&last_event_id)).await {
                Ok(Some(next)) => stream = Some(next),
                Ok(None) => break,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let reason = match failure {
            Some(error) => error.to_string(),
            None => "stream ended without a response".to_owned(),
        };
        self.events.emit_message(&JsonRpcMessage::Response {
            id: request_id,
            result: None,
            error: Some(crate::protocol::JsonRpcErrorObject {
                code: JSON_RPC_ERROR_INTERNAL,
                message: format!("MCP response stream failed: {reason}"),
                data: None,
            }),
        });
    }

    /// `runGetStream` (streamable-http.ts:452): keep the server-to-client
    /// stream open, reconnecting with backoff when it drops.
    async fn run_get_stream(self: Arc<Self>) {
        let mut cursor = StreamCursor::default();
        let mut attempt = 0u32;
        while !self.closed.load(Ordering::SeqCst) {
            match self.open_sse_stream(cursor.last_event_id.as_deref()).await {
                Ok(Some(stream)) => {
                    if self.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    let opened_at = tokio::time::Instant::now();
                    match self.consume_sse(stream, &mut cursor, None).await {
                        Ok(()) => {}
                        Err(error) => {
                            if self.closed.load(Ordering::SeqCst) {
                                return;
                            }
                            if !is_retryable(&error) {
                                self.events.emit_error(&error);
                                return;
                            }
                        }
                    }
                    // A stream that stayed up for a while counts as healthy,
                    // even if it was idle.
                    if cursor.received
                        || opened_at.elapsed()
                            > Duration::from_millis(self.options.reconnect.max_delay_ms)
                    {
                        attempt = 0;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    if self.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    if !is_retryable(&error) {
                        self.events.emit_error(&error);
                        return;
                    }
                }
            }
            cursor.received = false;
            if attempt >= self.options.reconnect.max_retries {
                self.events.emit_error(&McpTransportError::Other(
                    "MCP server-to-client stream dropped and could not be reopened".to_owned(),
                ));
                return;
            }
            let delay = self.reconnect_delay(attempt, cursor.retry_ms);
            attempt += 1;
            if !self.sleep(delay).await {
                return;
            }
        }
    }
}

/// `StreamableHttpTransport` (streamable-http.ts:216).
pub struct StreamableHttpTransport {
    shared: Arc<HttpShared>,
    started: AtomicBool,
    get_stream_started: AtomicBool,
}

impl StreamableHttpTransport {
    pub fn new(options: StreamableHttpTransportOptions) -> Result<Self, McpTransportError> {
        let url = Url::parse(&options.url).map_err(|error| {
            McpTransportError::Other(format!("invalid MCP server URL: {error}"))
        })?;
        let http = options.client.clone().unwrap_or_default();
        Ok(Self {
            shared: Arc::new(HttpShared {
                url,
                http,
                options,
                controller: CancellationToken::new(),
                closed: AtomicBool::new(false),
                session_id: Mutex::new(None),
                protocol_version: Mutex::new(None),
                events: Arc::new(TransportEvents::default()),
            }),
            started: AtomicBool::new(false),
            get_stream_started: AtomicBool::new(false),
        })
    }

    /// URL the transport posts to.
    pub fn url(&self) -> &Url {
        &self.shared.url
    }

    /// `get sessionId` (streamable-http.ts:240).
    pub fn session_id(&self) -> Option<String> {
        lock(&self.shared.session_id).clone()
    }

    fn start_get_stream(&self) {
        if !self.shared.options.open_get_stream
            || self.get_stream_started.swap(true, Ordering::SeqCst)
            || self.shared.closed.load(Ordering::SeqCst)
        {
            return;
        }
        let shared = self.shared.clone();
        tokio::spawn(async move { shared.run_get_stream().await });
    }
}

#[async_trait]
impl McpTransport for StreamableHttpTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(McpTransportError::Other(
                "MCP streamable HTTP transport already started".to_owned(),
            ));
        }
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        Ok(())
    }

    async fn send(&self, message: serde_json::Value) -> Result<(), McpTransportError> {
        if !self.started.load(Ordering::SeqCst) || self.shared.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        let body = serde_json::to_string(&message).map_err(|error| {
            McpTransportError::Other(format!("failed to serialize MCP message: {error}"))
        })?;
        let response = self
            .shared
            .authorized_fetch(
                reqwest::Method::POST,
                vec![
                    (
                        "accept".to_owned(),
                        "application/json, text/event-stream".to_owned(),
                    ),
                    ("content-type".to_owned(), "application/json".to_owned()),
                ],
                Some(body),
            )
            .await?;
        let response = self.shared.check_response(response).await?;
        self.shared.capture_session(&response);

        if !is_json_rpc_request(&message) {
            // Notifications and responses are acknowledged with 202 and carry
            // no reply; ignore any body. The server-to-client stream may only
            // open once the session is initialized.
            let is_initialized = message
                .get("method")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|method| method == "notifications/initialized");
            drop(response);
            if is_initialized {
                self.start_get_stream();
            }
            return Ok(());
        }
        if response.status() == reqwest::StatusCode::ACCEPTED
            || response.status() == reqwest::StatusCode::NO_CONTENT
        {
            return Err(http_error(
                response.status().as_u16(),
                "MCP server accepted request without a response".to_owned(),
                String::new(),
            ));
        }
        let content_type = content_type(&response);
        match content_type.as_deref() {
            Some("application/json") => {
                let body: serde_json::Value = response.json().await.map_err(map_reqwest_error)?;
                let messages = match body {
                    serde_json::Value::Array(items) => items,
                    other => vec![other],
                };
                for item in messages {
                    match parse_json_rpc_message(item) {
                        Some(message) => self.shared.events.emit_message(&message),
                        None => self.shared.events.emit_error(&McpTransportError::Other(
                            "MCP server sent an invalid JSON-RPC message".to_owned(),
                        )),
                    }
                }
                Ok(())
            }
            Some("text/event-stream") => {
                let request_id = message
                    .get("id")
                    .and_then(parse_json_rpc_id)
                    .expect("checked request");
                let shared = self.shared.clone();
                let stream = body_stream(response);
                tokio::spawn(async move {
                    shared.consume_response_stream(stream, request_id).await;
                });
                Ok(())
            }
            other => Err(http_error(
                response.status().as_u16(),
                format!(
                    "Unsupported MCP response content type: {}",
                    other.unwrap_or("missing")
                ),
                String::new(),
            )),
        }
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        if self.shared.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.shared.controller.cancel();
        // Best-effort session termination, bounded to 1s: header resolution
        // may run an OAuth refresh (up to 15s), and shutdown must not stall
        // on the token endpoint (v0.1.6 review P2-4). A cached token still
        // gets the DELETE; a mid-refresh cancel just skips it.
        if self.started.load(Ordering::SeqCst) && self.session_id().is_some() {
            let shared = self.shared.clone();
            let _ = tokio::time::timeout(Duration::from_millis(1_000), async move {
                if let Ok((headers, _token)) = shared.headers(&[]).await {
                    let request = shared
                        .http
                        .request(reqwest::Method::DELETE, shared.url.clone())
                        .headers(headers)
                        .timeout(Duration::from_millis(1_000));
                    let _ = request.send().await;
                }
            })
            .await;
        }
        self.shared.events.emit_close();
        Ok(())
    }

    fn set_protocol_version(&self, version: &str) {
        *lock(&self.shared.protocol_version) = Some(version.to_owned());
    }

    fn events(&self) -> Arc<TransportEvents> {
        self.shared.events.clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_events(input: &'static [u8]) -> (Vec<SseEvent>, Vec<String>, Vec<u64>) {
        let mut events = Vec::new();
        let mut ids = Vec::new();
        let mut retries = Vec::new();
        let stream = futures::stream::iter(vec![Ok(Bytes::from_static(input))]);
        let mut options = ConsumeSseOptions {
            max_event_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            on_event: &mut |event| events.push(event),
            on_id: &mut |id| ids.push(id.to_owned()),
            on_retry: &mut |delay| retries.push(delay),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(consume_sse_stream(Box::pin(stream), &mut options))
            .expect("sse parses");
        (events, ids, retries)
    }

    #[test]
    fn parses_sse_events_like_upstream() {
        let (events, ids, retries) = collect_events(
            b": comment\nevent: message\ndata: {\"a\":\nid: 7\nretry: 250\ndata: 1}\n\n",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "{\"a\":\n1}");
        assert_eq!(events[0].id.as_deref(), Some("7"));
        assert_eq!(ids, vec!["7".to_owned()]);
        assert_eq!(retries, vec![250]);
    }

    /// v0.1.6 review P3: a leading UTF-8 BOM must not eat the first event.
    #[test]
    fn strips_a_leading_utf8_bom() {
        let (events, _, _) = collect_events(b"\xef\xbb\xbfdata: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        // A split BOM across chunks matches too.
        let stream = futures::stream::iter(vec![
            Ok(Bytes::from_static(b"\xef\xbb")),
            Ok(Bytes::from_static(b"\xbfdata: hi\n\n")),
        ]);
        let mut events = Vec::new();
        let mut options = ConsumeSseOptions {
            max_event_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            on_event: &mut |event| events.push(event),
            on_id: &mut |_| {},
            on_retry: &mut |_| {},
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(consume_sse_stream(Box::pin(stream), &mut options))
            .expect("sse parses");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hi");
    }

    #[test]
    fn flushes_a_trailing_event_without_blank_line() {
        let (events, _, _) = collect_events(b"data: hello");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].event, None);
    }

    #[test]
    fn id_nul_and_bad_retry_are_ignored() {
        let (_, ids, retries) = collect_events(b"id: bad\0id\nretry: nope\ndata: x\n\n");
        assert!(ids.is_empty());
        assert!(retries.is_empty());
    }

    #[test]
    fn rejects_oversized_events() {
        let mut events = Vec::new();
        let mut options = ConsumeSseOptions {
            max_event_bytes: 4,
            on_event: &mut |event: SseEvent| events.push(event),
            on_id: &mut |_| {},
            on_retry: &mut |_| {},
        };
        let stream = futures::stream::iter(vec![Ok(Bytes::from_static(b"data: 12345\n\n"))]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        assert!(
            runtime
                .block_on(consume_sse_stream(Box::pin(stream), &mut options))
                .is_err()
        );
    }

    #[test]
    fn transient_statuses_match_upstream() {
        for status in [408, 429, 500, 501, 502] {
            assert!(is_transient_status(status));
        }
        for status in [400, 401, 403, 404, 405] {
            assert!(!is_transient_status(status));
        }
    }
}
