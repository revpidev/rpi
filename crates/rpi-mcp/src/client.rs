//! The MCP client session (port of `packages/mcp/src/client.ts` @
//! a13d35a74): initialize handshake, request/notification dispatching,
//! paginated listings, tool calls, progress and cancellation handling.
//!
//! The client is always used through an `Arc` (like the upstream runtime
//! holds one client per server): detached timeout/abort/cancellation tasks
//! need to outlive the calling borrow.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::content::{LlmContent, to_llm_content};
use crate::protocol::{
    JSON_RPC_ERROR_INTERNAL, JSON_RPC_ERROR_METHOD_NOT_FOUND, JsonRpcErrorObject, JsonRpcId,
    JsonRpcMessage, McpError,
};
use crate::transport::McpTransport;
use crate::types::{
    CallToolResult, InitializeResult, ListPage, ProgressNotification, Resource, ResourceTemplate,
    SUPPORTED_PROTOCOL_VERSIONS, Tool, validate_call_tool_result, validate_initialize_result,
    validate_list_page, validate_read_resource_result,
};

/// `DEFAULT_REQUEST_TIMEOUT_MS` (client.ts:40).
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
/// `MAX_LIST_PAGES` (client.ts:41).
const MAX_LIST_PAGES: usize = 1_000;

/// `McpClientOptions` (client.ts:60).
#[derive(Clone)]
pub struct McpClientOptions {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
    /// `ClientCapabilities` JSON; `roots` is added from [`Self::roots`].
    pub capabilities: Value,
    pub protocol_version: Option<String>,
    pub request_timeout_ms: Option<u64>,
    pub roots: Vec<crate::types::Root>,
}

impl McpClientOptions {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            title: None,
            capabilities: json!({}),
            protocol_version: None,
            request_timeout_ms: None,
            roots: Vec::new(),
        }
    }
}

/// `McpRequestOptions` (client.ts:68).
#[derive(Clone, Default)]
pub struct McpRequestOptions {
    /// Abort signal: cancels the request and, unless it is `initialize`,
    /// notifies the server.
    pub signal: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
    pub on_progress: Option<ProgressCallback>,
}

/// `onProgress` callback (client.ts:73).
pub type ProgressCallback = Arc<dyn Fn(&ProgressNotification) + Send + Sync>;

impl std::fmt::Debug for McpRequestOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRequestOptions")
            .field("signal", &self.signal)
            .field("timeout_ms", &self.timeout_ms)
            .field("on_progress", &self.on_progress.is_some())
            .finish()
    }
}

/// `ClientState` (client.ts:43).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    Idle,
    Connecting,
    Connected,
    Closed,
}

impl ClientState {
    fn as_str(self) -> &'static str {
        match self {
            ClientState::Idle => "idle",
            ClientState::Connecting => "connecting",
            ClientState::Connected => "connected",
            ClientState::Closed => "closed",
        }
    }
}

type HandlerFuture = Pin<Box<dyn Future<Output = Result<Value, McpError>> + Send>>;
type RequestHandler = Arc<dyn Fn(Value) -> HandlerFuture + Send + Sync>;
type NotificationListener = Arc<dyn Fn(&Value) + Send + Sync>;
type ErrorListener = Arc<dyn Fn(&McpError) + Send + Sync>;
type CloseListener = Arc<dyn Fn() + Send + Sync>;

/// RAII handle that removes one listener/handler when `dispose()` is
/// called. Dropping the guard alone keeps the listener (upstream returns an
/// unsubscribe function callers may ignore).
pub struct ListenerGuard {
    remove: Option<Box<dyn FnOnce() + Send>>,
}

impl ListenerGuard {
    fn new(remove: impl FnOnce() + Send + 'static) -> Self {
        Self {
            remove: Some(Box::new(remove)),
        }
    }

    pub fn dispose(mut self) {
        if let Some(remove) = self.remove.take() {
            remove();
        }
    }
}

struct PendingRequest {
    resolve: oneshot::Sender<Result<Value, McpError>>,
    timeout_ms: u64,
    /// Cancels the timeout/abort watches when the request settles.
    settled: CancellationToken,
    /// The live timeout timer; re-armed on progress (upstream `armTimeout`).
    timer: Option<CancellationToken>,
    cancellable: bool,
    on_progress: Option<ProgressCallback>,
    progress_token: Option<JsonRpcId>,
}

/// `McpClient` (client.ts:158).
pub struct McpClient {
    options: McpClientOptions,
    state: Mutex<ClientState>,
    transport: Mutex<Option<Arc<dyn McpTransport>>>,
    next_request_id: AtomicU64,
    next_listener_id: AtomicU64,
    server_info: Mutex<Option<crate::types::Implementation>>,
    server_capabilities: Mutex<Option<crate::types::ServerCapabilities>>,
    instructions: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    pending: Mutex<HashMap<JsonRpcId, PendingRequest>>,
    progress_requests: Mutex<HashMap<JsonRpcId, JsonRpcId>>,
    incoming: Mutex<HashMap<JsonRpcId, CancellationToken>>,
    request_handlers: Mutex<HashMap<String, RequestHandler>>,
    notification_listeners: Mutex<HashMap<String, Vec<(u64, NotificationListener)>>>,
    error_listeners: Mutex<Vec<(u64, ErrorListener)>>,
    close_listeners: Mutex<Vec<(u64, CloseListener)>>,
    disposers: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    closed_emitted: Mutex<bool>,
}

impl McpClient {
    pub fn new(options: McpClientOptions) -> Arc<Self> {
        let mut request_handlers: HashMap<String, RequestHandler> = HashMap::new();
        request_handlers.insert(
            "ping".to_owned(),
            Arc::new(|_params| Box::pin(async { Ok(json!({})) })),
        );
        if !options.roots.is_empty() {
            let roots: Vec<Value> = options
                .roots
                .iter()
                .map(|root| serde_json::to_value(root).unwrap_or(Value::Null))
                .collect();
            request_handlers.insert(
                "roots/list".to_owned(),
                Arc::new(move |_params| {
                    let roots = roots.clone();
                    Box::pin(async move { Ok(json!({ "roots": roots })) })
                }),
            );
        }
        Arc::new(Self {
            options,
            state: Mutex::new(ClientState::Idle),
            transport: Mutex::new(None),
            next_request_id: AtomicU64::new(1),
            next_listener_id: AtomicU64::new(1),
            server_info: Mutex::new(None),
            server_capabilities: Mutex::new(None),
            instructions: Mutex::new(None),
            protocol_version: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            progress_requests: Mutex::new(HashMap::new()),
            incoming: Mutex::new(HashMap::new()),
            request_handlers: Mutex::new(request_handlers),
            notification_listeners: Mutex::new(HashMap::new()),
            error_listeners: Mutex::new(Vec::new()),
            close_listeners: Mutex::new(Vec::new()),
            disposers: Mutex::new(Vec::new()),
            closed_emitted: Mutex::new(false),
        })
    }

    /// `get connectionState` (client.ts:196).
    pub fn connection_state(&self) -> ClientState {
        *lock(&self.state)
    }

    /// `get serverInfo` (client.ts:200).
    pub fn server_info(&self) -> Option<crate::types::Implementation> {
        lock(&self.server_info).clone()
    }

    /// `get serverCapabilities` (client.ts:204).
    pub fn server_capabilities(&self) -> Option<crate::types::ServerCapabilities> {
        lock(&self.server_capabilities).clone()
    }

    /// `get instructions` (client.ts:208).
    pub fn instructions(&self) -> Option<String> {
        lock(&self.instructions).clone()
    }

    /// `get protocolVersion` (client.ts:212).
    pub fn protocol_version(&self) -> Option<String> {
        lock(&self.protocol_version).clone()
    }

    /// `connect(transport)` (client.ts:216).
    pub async fn connect(
        self: &Arc<Self>,
        transport: Arc<dyn McpTransport>,
    ) -> Result<InitializeResult, McpError> {
        if self.connection_state() != ClientState::Idle {
            return Err(McpError::Invalid(format!(
                "Cannot connect MCP client in {} state",
                self.connection_state().as_str()
            )));
        }
        *lock(&self.state) = ClientState::Connecting;
        *lock(&self.transport) = Some(transport.clone());
        {
            let events = transport.events();
            let on_message = {
                let client = self.clone();
                events.on_message(Arc::new(move |message| client.handle_message(message)))
            };
            let on_error = {
                let client = self.clone();
                events.on_error(Arc::new(move |error| {
                    client.emit_error(McpError::Transport(error.to_string()))
                }))
            };
            let on_close = {
                let client = self.clone();
                events.on_close(Arc::new(move || client.handle_transport_close()))
            };
            let mut disposers = lock(&self.disposers);
            disposers.push(Box::new(move || drop(on_message)));
            disposers.push(Box::new(move || drop(on_error)));
            disposers.push(Box::new(move || drop(on_close)));
        }

        let result: Result<InitializeResult, McpError> = async {
            transport
                .start()
                .await
                .map_err(|error| error.into_mcp_error())?;
            let mut capabilities = self.options.capabilities.clone();
            if !self.options.roots.is_empty()
                && capabilities.get("roots").is_none_or(Value::is_null)
                && let Some(map) = capabilities.as_object_mut()
            {
                map.insert("roots".to_owned(), json!({}));
            }
            let mut client_info = json!({
                "name": self.options.name,
                "version": self.options.version,
            });
            if let Some(title) = &self.options.title {
                client_info["title"] = Value::String(title.clone());
            }
            let params = json!({
                "protocolVersion": self
                    .options
                    .protocol_version
                    .clone()
                    .unwrap_or_else(|| crate::types::LATEST_PROTOCOL_VERSION.to_owned()),
                "capabilities": capabilities,
                "clientInfo": client_info,
            });
            let result = self
                .request_internal(
                    "initialize",
                    Some(params),
                    &McpRequestOptions::default(),
                    true,
                )
                .await?;
            let result = validate_initialize_result(&result)?;
            if !SUPPORTED_PROTOCOL_VERSIONS.contains(&result.protocol_version.as_str()) {
                return Err(McpError::Invalid(format!(
                    "MCP server selected unsupported protocol version {}",
                    result.protocol_version
                )));
            }
            *lock(&self.protocol_version) = Some(result.protocol_version.clone());
            *lock(&self.server_info) = Some(result.server_info.clone());
            *lock(&self.server_capabilities) = Some(result.capabilities.clone());
            *lock(&self.instructions) = result.instructions.clone();
            transport.set_protocol_version(&result.protocol_version);
            self.notify_internal("notifications/initialized", None, true)
                .await?;
            *lock(&self.state) = ClientState::Connected;
            Ok(result)
        }
        .await;

        if result.is_err() {
            let _ = self.close().await;
        }
        result
    }

    /// `request()` (client.ts:267).
    pub async fn request(
        self: &Arc<Self>,
        method: &str,
        params: Option<Value>,
        options: McpRequestOptions,
    ) -> Result<Value, McpError> {
        self.request_internal(method, params, &options, false).await
    }

    /// `notify()` (client.ts:275).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        self.notify_internal(method, params, false).await
    }

    /// `setRequestHandler()` (client.ts:279). The guard removes the handler
    /// it installed (only when it was not replaced meanwhile).
    pub fn set_request_handler(
        self: &Arc<Self>,
        method: &str,
        handler: RequestHandler,
    ) -> ListenerGuard {
        lock(&self.request_handlers).insert(method.to_owned(), handler.clone());
        let client = self.clone();
        let method = method.to_owned();
        ListenerGuard::new(move || {
            let mut handlers = lock(&client.request_handlers);
            let same = handlers
                .get(&method)
                .is_some_and(|current| Arc::ptr_eq(current, &handler));
            if same {
                handlers.remove(&method);
            }
        })
    }

    /// `onNotification()` (client.ts:287).
    pub fn on_notification(
        self: &Arc<Self>,
        method: &str,
        listener: NotificationListener,
    ) -> ListenerGuard {
        let id = self.next_listener_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.notification_listeners)
            .entry(method.to_owned())
            .or_default()
            .push((id, listener));
        let client = self.clone();
        let method = method.to_owned();
        ListenerGuard::new(move || {
            let mut listeners = lock(&client.notification_listeners);
            if let Some(entries) = listeners.get_mut(&method) {
                entries.retain(|(existing, _)| *existing != id);
                if entries.is_empty() {
                    listeners.remove(&method);
                }
            }
        })
    }

    /// `onError()` (client.ts:298).
    pub fn on_error(self: &Arc<Self>, listener: ErrorListener) -> ListenerGuard {
        let id = self.next_listener_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.error_listeners).push((id, listener));
        let client = self.clone();
        ListenerGuard::new(move || {
            lock(&client.error_listeners).retain(|(existing, _)| *existing != id);
        })
    }

    /// `onClose()` (client.ts:304): called once when the connection closes,
    /// whether the transport dropped or `close()` was called.
    pub fn on_close(self: &Arc<Self>, listener: CloseListener) -> ListenerGuard {
        let id = self.next_listener_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.close_listeners).push((id, listener));
        let client = self.clone();
        ListenerGuard::new(move || {
            lock(&client.close_listeners).retain(|(existing, _)| *existing != id);
        })
    }

    /// `ping()` (client.ts:311).
    pub async fn ping(self: &Arc<Self>, options: McpRequestOptions) -> Result<(), McpError> {
        self.request("ping", None, options).await.map(|_| ())
    }

    /// `armTimeout` (client.ts:497): (re)start the request's timeout timer,
    /// cancelling any previous one.
    fn arm_timeout(self: &Arc<Self>, id: &JsonRpcId) {
        let (timer, timeout_ms, cancellable) = {
            let mut pending = lock(&self.pending);
            let Some(entry) = pending.get_mut(id) else {
                return;
            };
            if let Some(previous) = entry.timer.take() {
                previous.cancel();
            }
            let timer = entry.settled.child_token();
            entry.timer = Some(timer.clone());
            (timer, entry.timeout_ms, entry.cancellable)
        };
        let client = self.clone();
        let id = id.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                    client.cancel_pending(
                        &id,
                        McpError::Timeout { timeout_ms },
                        cancellable,
                        Some("Request timed out".to_owned()),
                    );
                }
                () = timer.cancelled() => {}
            }
        });
    }

    /// `listTools()` (client.ts:315).
    pub async fn list_tools(
        self: &Arc<Self>,
        options: McpRequestOptions,
    ) -> Result<Vec<Tool>, McpError> {
        let items = self
            .list_all("tools/list", "tools", Tool::is_tool, options)
            .await?;
        items.iter().map(Tool::parse).collect()
    }

    /// `listResources()` (client.ts:320).
    pub async fn list_resources(
        self: &Arc<Self>,
        options: McpRequestOptions,
    ) -> Result<Vec<Resource>, McpError> {
        let items = self
            .list_all(
                "resources/list",
                "resources",
                Resource::is_resource,
                options,
            )
            .await?;
        items.iter().map(Resource::parse).collect()
    }

    /// `listResourcesPage()` (client.ts:325).
    pub async fn list_resources_page(
        self: &Arc<Self>,
        cursor: Option<String>,
        options: McpRequestOptions,
    ) -> Result<(Vec<Resource>, Option<String>), McpError> {
        let page = self
            .list_page(
                "resources/list",
                "resources",
                Resource::is_resource,
                cursor,
                options,
            )
            .await?;
        let resources = page
            .items
            .iter()
            .map(Resource::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((resources, page.next_cursor))
    }

    /// `listResourceTemplates()` (client.ts:331).
    pub async fn list_resource_templates(
        self: &Arc<Self>,
        options: McpRequestOptions,
    ) -> Result<Vec<ResourceTemplate>, McpError> {
        let items = self
            .list_all(
                "resources/templates/list",
                "resourceTemplates",
                ResourceTemplate::is_resource_template,
                options,
            )
            .await?;
        items.iter().map(ResourceTemplate::parse).collect()
    }

    /// `listResourceTemplatesPage()` (client.ts:340).
    pub async fn list_resource_templates_page(
        self: &Arc<Self>,
        cursor: Option<String>,
        options: McpRequestOptions,
    ) -> Result<(Vec<ResourceTemplate>, Option<String>), McpError> {
        let page = self
            .list_page(
                "resources/templates/list",
                "resourceTemplates",
                ResourceTemplate::is_resource_template,
                cursor,
                options,
            )
            .await?;
        let templates = page
            .items
            .iter()
            .map(ResourceTemplate::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((templates, page.next_cursor))
    }

    /// `readResource()` (client.ts:355).
    pub async fn read_resource(
        self: &Arc<Self>,
        uri: &str,
        options: McpRequestOptions,
    ) -> Result<Value, McpError> {
        let result = self
            .request("resources/read", Some(json!({ "uri": uri })), options)
            .await?;
        validate_read_resource_result(&result)?;
        Ok(result)
    }

    /// `callTool()` (client.ts:411).
    pub async fn call_tool(
        self: &Arc<Self>,
        name: &str,
        args: Option<Value>,
        options: McpRequestOptions,
    ) -> Result<CallToolResult, McpError> {
        let mut params = serde_json::Map::new();
        params.insert("name".to_owned(), Value::String(name.to_owned()));
        if let Some(args) = args {
            params.insert("arguments".to_owned(), args);
        }
        let result = self
            .request("tools/call", Some(Value::Object(params)), options)
            .await?;
        validate_call_tool_result(&result)
    }

    /// Convenience: the LLM-facing text/image conversion of a tool result.
    pub fn call_tool_llm_content(result: &CallToolResult) -> Vec<LlmContent> {
        to_llm_content(result)
    }

    /// `close()` (client.ts:419): idempotent.
    pub async fn close(self: &Arc<Self>) -> Result<(), McpError> {
        let transport = lock(&self.transport).take();
        for dispose in lock(&self.disposers).drain(..) {
            dispose();
        }
        self.mark_closed(McpError::connection_closed());
        if let Some(transport) = transport {
            transport
                .close()
                .await
                .map_err(|error| error.into_mcp_error())?;
        }
        Ok(())
    }

    // -- internals ---------------------------------------------------------

    async fn request_internal(
        self: &Arc<Self>,
        method: &str,
        params: Option<Value>,
        options: &McpRequestOptions,
        allow_connecting: bool,
    ) -> Result<Value, McpError> {
        let transport = self.require_transport(allow_connecting)?;
        if options
            .signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(McpError::Aborted);
        }
        let id = JsonRpcId::Number(self.next_request_id.fetch_add(1, Ordering::SeqCst).into());
        let progress_token = options.on_progress.is_some().then(|| id.clone());
        let request_params = match &progress_token {
            None => params,
            Some(token) => {
                let mut params = params.unwrap_or_else(|| json!({}));
                if !params.is_object() {
                    params = json!({});
                }
                let meta = params
                    .as_object_mut()
                    .expect("object")
                    .entry("_meta")
                    .or_insert_with(|| json!({}));
                if !meta.is_object() {
                    *meta = json!({});
                }
                meta.as_object_mut().expect("object").insert(
                    "progressToken".to_owned(),
                    serde_json::to_value(token).unwrap_or(Value::Null),
                );
                Some(params)
            }
        };
        let mut message = json!({
            "jsonrpc": "2.0",
            "id": serde_json::to_value(&id).unwrap_or(Value::Null),
            "method": method,
        });
        if let Some(params) = &request_params {
            message["params"] = params.clone();
        }
        let timeout_ms = options
            .timeout_ms
            .or(self.options.request_timeout_ms)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS);
        let (resolve, receiver) = oneshot::channel();
        let entry = PendingRequest {
            resolve,
            timeout_ms,
            settled: CancellationToken::new(),
            timer: None,
            cancellable: method != "initialize",
            on_progress: options.on_progress.clone(),
            progress_token: progress_token.clone(),
        };
        let cancellable = method != "initialize";
        let settled = entry.settled.clone();
        lock(&self.pending).insert(id.clone(), entry);
        self.arm_timeout(&id);
        if let Some(token) = &progress_token {
            lock(&self.progress_requests).insert(token.clone(), id.clone());
        }
        if let Some(signal) = &options.signal {
            let signal = signal.clone();
            let client = self.clone();
            let id = id.clone();
            let settled = settled.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = signal.cancelled() => {
                        client.cancel_pending(&id, McpError::Aborted, cancellable, Some("Aborted".to_owned()));
                    }
                    () = settled.cancelled() => {}
                }
            });
        }
        if let Err(error) = transport.send(message).await {
            self.cancel_pending(&id, error.into_mcp_error(), false, None);
        }
        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(McpError::connection_closed()),
        }
    }

    async fn notify_internal(
        &self,
        method: &str,
        params: Option<Value>,
        allow_connecting: bool,
    ) -> Result<(), McpError> {
        let transport = self.require_transport(allow_connecting)?;
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        transport
            .send(message)
            .await
            .map_err(|error| error.into_mcp_error())
    }

    fn require_transport(&self, allow_connecting: bool) -> Result<Arc<dyn McpTransport>, McpError> {
        let state = self.connection_state();
        let transport = lock(&self.transport).clone();
        match (transport, state) {
            (Some(transport), ClientState::Connected) => Ok(transport),
            (Some(transport), ClientState::Connecting) if allow_connecting => Ok(transport),
            (_, state) => Err(McpError::ConnectionClosed(format!(
                "MCP client is {}",
                state.as_str()
            ))),
        }
    }

    async fn list_page(
        self: &Arc<Self>,
        method: &str,
        key: &str,
        is_item: impl Fn(&Value) -> bool,
        cursor: Option<String>,
        options: McpRequestOptions,
    ) -> Result<ListPage, McpError> {
        let params = cursor.map(|cursor| json!({ "cursor": cursor }));
        let value = self.request(method, params, options).await?;
        validate_list_page(method, key, &value, is_item)
    }

    /// `listAll` (client.ts:376): every page of a paginated list method.
    async fn list_all(
        self: &Arc<Self>,
        method: &str,
        key: &str,
        is_item: impl Fn(&Value) -> bool,
        options: McpRequestOptions,
    ) -> Result<Vec<Value>, McpError> {
        let mut items: Vec<Value> = Vec::new();
        let mut cursors: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let page = self
                .list_page(method, key, &is_item, cursor.clone(), options.clone())
                .await?;
            items.extend(page.items);
            match page.next_cursor {
                None => return Ok(items),
                Some(next) => {
                    if !cursors.insert(next.clone()) {
                        return Err(McpError::Invalid(format!(
                            "MCP {method} returned duplicate cursor: {next}"
                        )));
                    }
                    cursor = Some(next);
                }
            }
        }
        Err(McpError::Invalid(format!(
            "MCP {method} exceeded {MAX_LIST_PAGES} pages"
        )))
    }

    fn cancel_pending(
        self: &Arc<Self>,
        id: &JsonRpcId,
        error: McpError,
        notify_server: bool,
        reason: Option<String>,
    ) {
        let Some(entry) = lock(&self.pending).remove(id) else {
            return;
        };
        entry.settled.cancel();
        if let Some(token) = &entry.progress_token {
            lock(&self.progress_requests).remove(token);
        }
        let _ = entry.resolve.send(Err(error));
        if notify_server && let Some(transport) = lock(&self.transport).clone() {
            let mut params =
                json!({ "requestId": serde_json::to_value(id).unwrap_or(Value::Null) });
            if let Some(reason) = reason {
                params["reason"] = Value::String(reason);
            }
            let client = self.clone();
            tokio::spawn(async move {
                if let Err(error) = transport
                    .send(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": params,
                    }))
                    .await
                {
                    client.emit_error(McpError::Transport(error.to_string()));
                }
            });
        }
    }

    fn handle_message(self: &Arc<Self>, message: &JsonRpcMessage) {
        match message {
            JsonRpcMessage::Response { id, result, error } => {
                self.handle_response(id, result, error)
            }
            JsonRpcMessage::Request { id, method, params } => {
                let client = self.clone();
                let id = id.clone();
                let method = method.clone();
                let params = params.clone();
                tokio::spawn(async move { client.handle_request(id, method, params).await });
            }
            JsonRpcMessage::Notification { method, params } => {
                self.handle_notification(method, params)
            }
        }
    }

    /// Remove the pending entry for `id`, matching `1` and `1.0` as the
    /// same JavaScript number (v0.1.6 review P3): the exact-key miss used
    /// to leave a response unclaimed until its timeout fired.
    fn take_pending(&self, id: &JsonRpcId) -> Option<PendingRequest> {
        let mut pending = lock(&self.pending);
        if let Some(entry) = pending.remove(id) {
            return Some(entry);
        }
        let key = pending.keys().find(|key| key.numerically_eq(id)).cloned()?;
        pending.remove(&key)
    }

    fn handle_response(
        &self,
        id: &JsonRpcId,
        result: &Option<Value>,
        error: &Option<JsonRpcErrorObject>,
    ) {
        let Some(entry) = self.take_pending(id) else {
            self.emit_error(McpError::Invalid(format!(
                "Received response for unknown MCP request {id}"
            )));
            return;
        };
        entry.settled.cancel();
        if let Some(token) = &entry.progress_token {
            lock(&self.progress_requests).remove(token);
        }
        let outcome = match error {
            Some(error) => Err(McpError::Rpc {
                code: error.code,
                message: error.message.clone(),
                data: error.data.clone(),
            }),
            None => Ok(result.clone().unwrap_or(Value::Null)),
        };
        let _ = entry.resolve.send(outcome);
    }

    async fn handle_request(self: Arc<Self>, id: JsonRpcId, method: String, params: Value) {
        let Some(transport) = lock(&self.transport).clone() else {
            return;
        };
        let handler = lock(&self.request_handlers).get(&method).cloned();
        let Some(handler) = handler else {
            if let Err(error) = transport
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": JSON_RPC_ERROR_METHOD_NOT_FOUND,
                        "message": format!("Method not found: {method}"),
                    },
                }))
                .await
            {
                self.emit_error(McpError::Transport(error.to_string()));
            }
            return;
        };
        let controller = CancellationToken::new();
        lock(&self.incoming).insert(id.clone(), controller.clone());
        let outcome = tokio::select! {
            result = handler(params) => result,
            () = controller.cancelled() => Err(McpError::Aborted),
        };
        lock(&self.incoming).remove(&id);
        let response = match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => match error {
                McpError::Rpc {
                    code,
                    message,
                    data,
                } => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message, "data": data },
                }),
                other => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": JSON_RPC_ERROR_INTERNAL, "message": other.to_string() },
                }),
            },
        };
        if let Err(error) = transport.send(response).await {
            self.emit_error(McpError::Transport(error.to_string()));
        }
    }

    fn handle_notification(self: &Arc<Self>, method: &str, params: &Value) {
        if method == "notifications/progress" {
            self.handle_progress(params);
        } else if method == "notifications/cancelled" {
            self.handle_cancelled(params);
        }
        let listeners = lock(&self.notification_listeners)
            .get(method)
            .map(|entries| {
                entries
                    .iter()
                    .map(|(_, listener)| listener.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for listener in listeners {
            listener(params);
        }
    }

    /// `handleProgress` (client.ts:466): re-arms the request timeout.
    fn handle_progress(self: &Arc<Self>, params: &Value) {
        let Some(token) = params
            .get("progressToken")
            .and_then(crate::protocol::parse_json_rpc_id)
        else {
            return;
        };
        if params.get("progress").and_then(Value::as_f64).is_none() {
            return;
        }
        let Some(request_id) = lock(&self.progress_requests).get(&token).cloned() else {
            return;
        };
        let on_progress = {
            let pending = lock(&self.pending);
            match pending.get(&request_id) {
                Some(entry) => entry.on_progress.clone(),
                None => return,
            }
        };
        // `handleProgress` re-arms the request timeout.
        self.arm_timeout(&request_id);
        if let Some(callback) = on_progress
            && let Ok(progress) = serde_json::from_value::<ProgressNotification>(params.clone())
        {
            callback(&progress);
        }
    }

    /// `handleCancelled` (client.ts:483).
    fn handle_cancelled(&self, params: &Value) {
        let Some(request_id) = params.get("requestId").filter(|value| !value.is_null()) else {
            return;
        };
        let Some(controller) = crate::protocol::parse_json_rpc_id(request_id)
            .and_then(|id| lock(&self.incoming).get(&id).cloned())
        else {
            return;
        };
        controller.cancel();
    }

    fn handle_transport_close(self: &Arc<Self>) {
        self.mark_closed(McpError::connection_closed());
    }

    /// `markClosed` (client.ts:514): idempotent.
    fn mark_closed(&self, error: McpError) {
        let was_closed = {
            let mut state = lock(&self.state);
            let was_closed = *state == ClientState::Closed;
            *state = ClientState::Closed;
            was_closed
        };
        for (_, entry) in lock(&self.pending).drain() {
            entry.settled.cancel();
            let _ = entry.resolve.send(Err(error.clone()));
        }
        for (_, controller) in lock(&self.incoming).drain() {
            controller.cancel();
        }
        lock(&self.progress_requests).clear();
        if was_closed {
            return;
        }
        if *lock(&self.closed_emitted) {
            return;
        }
        *lock(&self.closed_emitted) = true;
        for (_, listener) in lock(&self.close_listeners).clone() {
            listener();
        }
    }

    fn emit_error(&self, error: McpError) {
        for (_, listener) in lock(&self.error_listeners).clone() {
            listener(&error);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
