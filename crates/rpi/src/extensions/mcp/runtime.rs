//! The part of the MCP integration that talks to servers: connections,
//! transports and OAuth (port of
//! `packages/coding-agent/src/extensions/mcp/runtime.ts` @ a13d35a74).
//!
//! Connections run in the background; calls reconnect lazily when the
//! connection went away. Read-only requests retry once after a transient
//! HTTP error, and a session-expired answer retries once on a fresh
//! session. A server that needs a sign-in reports it through
//! [`McpError::AuthorizationRequired`].

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use rpi_mcp::oauth::{OAuthChallenge, parse_www_authenticate};
use rpi_mcp::types::{CallToolResult, ListPage, Resource, ResourceTemplate, Tool as McpTool};
use rpi_mcp::{
    AuthProvider, ClientState, McpClient, McpClientOptions, McpError, McpRequestOptions,
    McpTransport, Root, StderrMode, StdioTransport, StdioTransportOptions, StreamableHttpTransport,
    StreamableHttpTransportOptions,
};
use tokio::sync::Mutex as AsyncMutex;

use super::config::{McpServerConfig, McpServerEntry, mcp_namespace};
use super::log::McpServerLog;
use super::oauth::{McpAuthProvider, McpOAuthCredentialStore, McpOAuthSettings};
use super::resources::McpResourceServer;
use super::tools::{McpCallOptions, McpToolCaller};

const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;
const STDERR_TAIL_CHARS: usize = 2_000;
/// Delays between attempts to connect to an HTTP server that failed with a
/// transient error (`CONNECT_RETRY_DELAYS_MS`, runtime.ts:50).
const CONNECT_RETRY_DELAYS_MS: [u64; 2] = [250, 1_000];

/// `ServerState` (runtime.ts:56).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Connecting,
    Connected,
    Disconnected,
    NeedsAuth,
    Failed,
    Closed,
}

impl ServerState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServerState::Connecting => "connecting",
            ServerState::Connected => "connected",
            ServerState::Disconnected => "disconnected",
            ServerState::NeedsAuth => "needs-auth",
            ServerState::Failed => "failed",
            ServerState::Closed => "closed",
        }
    }
}

/// `McpTransportFactory` (runtime.ts:62).
pub type McpTransportFactory = Arc<
    dyn Fn(
            &McpServerEntry,
            &str,
            Option<Arc<dyn AuthProvider>>,
        ) -> Result<Arc<dyn McpTransport>, String>
        + Send
        + Sync,
>;

/// Reads the current token of a pi provider (`auth.provider`).
pub type ProviderTokenFn =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Option<String>> + Send + Sync>;

/// Callback invoked with the connection whose state or tools changed.
pub type ConnectionCallback = Arc<dyn Fn(&McpServerConnection) + Send + Sync>;

/// `expandHome` (runtime.ts:84).
fn expand_home(value: &str) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if value == "~" {
        return home
            .map(|home| home.display().to_string())
            .unwrap_or_else(|| value.to_owned());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return match home {
            Some(home) => home.join(rest).display().to_string(),
            None => value.to_owned(),
        };
    }
    value.to_owned()
}

/// `createDefaultTransport` (runtime.ts:92).
pub fn create_default_transport(
    entry: &McpServerEntry,
    cwd: &str,
    auth_provider: Option<Arc<dyn AuthProvider>>,
) -> Result<Arc<dyn McpTransport>, String> {
    let name = &entry.name;
    match &entry.config {
        McpServerConfig::Http(config) => {
            let mut headers: rpi_ai::types::ProviderHeaders = HashMap::new();
            for (key, value) in config.headers.clone().unwrap_or_default() {
                headers.insert(key, value.as_str().map(str::to_owned));
            }
            let resolved = rpi_ai::auth::config_value::resolve_headers_or_throw(
                Some(&headers),
                &format!("MCP server \"{name}\""),
                None,
            )
            .map_err(|error| error.to_string())?;
            let mut options = StreamableHttpTransportOptions::new(config.url.clone());
            options.headers = resolved
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(key, value)| value.map(|value| (key, value)))
                .collect();
            options.auth_provider = auth_provider;
            let transport =
                StreamableHttpTransport::new(options).map_err(|error| error.to_string())?;
            Ok(Arc::new(transport))
        }
        McpServerConfig::Stdio(config) => {
            let mut env = HashMap::new();
            for (key, value) in config.env.clone().unwrap_or_default() {
                let Some(value) = value.as_str() else {
                    continue;
                };
                let resolved = rpi_ai::auth::config_value::resolve_config_value_or_throw(
                    value,
                    &format!("MCP server \"{name}\" env \"{key}\""),
                    None,
                )
                .map_err(|error| error.to_string())?;
                env.insert(key, resolved);
            }
            let mut options = StdioTransportOptions::new(expand_home(&config.command));
            options.args = config
                .args
                .clone()
                .unwrap_or_default()
                .iter()
                .map(|arg| expand_home(arg))
                .collect();
            let relative_cwd = expand_home(config.cwd.as_deref().unwrap_or("."));
            let resolved_cwd = PathBuf::from(cwd);
            options.cwd = Some(resolved_cwd.join(relative_cwd).display().to_string());
            options.env = env;
            options.stderr = StderrMode::Pipe;
            Ok(Arc::new(StdioTransport::new(options)))
        }
    }
}

/// `signInRequiredMessage` (runtime.ts:78).
fn sign_in_required_message(entry: &McpServerEntry) -> String {
    let provider = match &entry.config {
        McpServerConfig::Http(config) => config.auth.as_ref().map(|auth| auth.provider.clone()),
        McpServerConfig::Stdio(_) => None,
    };
    match provider {
        Some(provider) => format!(
            "MCP server \"{}\" requires sign-in. Run /login {provider} to sign in.",
            entry.name
        ),
        None => format!(
            "MCP server \"{}\" requires sign-in. Run /mcp to sign in.",
            entry.name
        ),
    }
}

/// `usesOAuth` (runtime.ts:87): HTTP servers authenticate with OAuth unless
/// the config supplies an `Authorization` header or `auth`.
pub fn uses_oauth(entry: &McpServerEntry) -> bool {
    let McpServerConfig::Http(config) = &entry.config else {
        return false;
    };
    if config.auth.is_some() {
        return false;
    }
    !config
        .headers
        .clone()
        .unwrap_or_default()
        .keys()
        .any(|header| header.eq_ignore_ascii_case("authorization"))
}

/// `withoutTemplates` (runtime.ts:116): servers that do not implement
/// `resources/templates/list` have no templates.
async fn without_templates<T>(
    list: impl Future<Output = Result<T, McpError>>,
    empty: T,
) -> Result<T, McpError> {
    match list.await {
        Ok(value) => Ok(value),
        Err(error) if error.rpc_code() == Some(-32601) => Ok(empty),
        Err(error) => Err(error),
    }
}

/// A pi provider token seam for `auth.provider` (runtime.ts:143).
struct ProviderAuthProvider {
    provider: String,
    token: ProviderTokenFn,
}

#[async_trait::async_trait]
impl AuthProvider for ProviderAuthProvider {
    async fn token(&self) -> Option<String> {
        (self.token)(self.provider.clone()).await
    }
}

/// `entry_oauth` (runtime.ts:216).
fn entry_oauth(entry: &McpServerEntry) -> Option<&super::config::McpOAuthConfig> {
    match &entry.config {
        McpServerConfig::Http(config) => config.oauth.as_ref(),
        McpServerConfig::Stdio(_) => None,
    }
}

/// One configured server; reconnects lazily when a call finds the connection
/// gone (`McpServerConnection`, runtime.ts:136).
pub struct McpServerConnection {
    entry: McpServerEntry,
    state: Mutex<ServerState>,
    error: Mutex<Option<String>>,
    tools: Mutex<Vec<McpTool>>,
    has_resources: AtomicBool,
    resources: Mutex<Vec<Resource>>,
    resource_templates: Mutex<Vec<ResourceTemplate>>,
    instructions: Mutex<Option<String>>,
    challenge: Arc<Mutex<Option<OAuthChallenge>>>,
    client: AsyncMutex<Option<Arc<McpClient>>>,
    opening: AsyncMutex<()>,
    closed: AtomicBool,
    stderr_tail: Mutex<Option<String>>,
    cwd: String,
    create_transport: McpTransportFactory,
    auth_provider: Option<Arc<McpAuthProvider>>,
    provider_auth: Option<Arc<dyn AuthProvider>>,
    on_tools: ConnectionCallback,
    on_change: Option<ConnectionCallback>,
    log: Option<Arc<McpServerLog>>,
    /// Bumped so late refresh results from an old client are dropped.
    generation: AtomicU64,
}

/// Options for [`McpServerConnection::new`].
pub struct McpServerConnectionOptions {
    pub entry: McpServerEntry,
    pub cwd: String,
    pub create_transport: McpTransportFactory,
    pub credentials: Arc<McpOAuthCredentialStore>,
    pub provider_token: Option<ProviderTokenFn>,
    pub on_tools: ConnectionCallback,
    pub on_change: Option<ConnectionCallback>,
    pub log: Option<Arc<McpServerLog>>,
}

impl McpServerConnection {
    pub fn new(options: McpServerConnectionOptions) -> Self {
        let challenge: Arc<Mutex<Option<OAuthChallenge>>> = Arc::new(Mutex::new(None));
        let auth_provider = if uses_oauth(&options.entry) {
            let url = options.entry.config.url().unwrap_or_default().to_owned();
            let entry = options.entry.clone();
            let on_challenge = {
                let challenge = challenge.clone();
                Arc::new(move |value: &OAuthChallenge| {
                    *lock(&challenge) = Some(value.clone());
                })
            };
            Some(Arc::new(McpAuthProvider::new(
                url.clone(),
                options.credentials.for_server(&options.entry.name, &url),
                Arc::new(move || McpOAuthSettings::from_config(entry_oauth(&entry))),
                on_challenge,
            )))
        } else {
            None
        };
        let provider_auth = match &options.entry.config {
            McpServerConfig::Http(config) => config.auth.as_ref().map(|auth| {
                Arc::new(ProviderAuthProvider {
                    provider: auth.provider.clone(),
                    token: options
                        .provider_token
                        .clone()
                        .unwrap_or_else(|| Arc::new(|_| Box::pin(async { None }))),
                }) as Arc<dyn AuthProvider>
            }),
            McpServerConfig::Stdio(_) => None,
        };
        Self {
            entry: options.entry,
            state: Mutex::new(ServerState::Connecting),
            error: Mutex::new(None),
            tools: Mutex::new(Vec::new()),
            has_resources: AtomicBool::new(false),
            resources: Mutex::new(Vec::new()),
            resource_templates: Mutex::new(Vec::new()),
            instructions: Mutex::new(None),
            challenge,
            client: AsyncMutex::new(None),
            opening: AsyncMutex::new(()),
            closed: AtomicBool::new(false),
            stderr_tail: Mutex::new(None),
            cwd: options.cwd,
            create_transport: options.create_transport,
            auth_provider,
            provider_auth,
            on_tools: options.on_tools,
            on_change: options.on_change,
            log: options.log,
            generation: AtomicU64::new(0),
        }
    }

    pub fn name(&self) -> &str {
        &self.entry.name
    }

    pub fn entry(&self) -> &McpServerEntry {
        &self.entry
    }

    pub fn namespace(&self) -> String {
        mcp_namespace(&self.entry.name)
    }

    pub fn state(&self) -> ServerState {
        *lock(&self.state)
    }

    pub fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }

    pub fn tools(&self) -> Vec<McpTool> {
        lock(&self.tools).clone()
    }

    pub fn has_resources(&self) -> bool {
        self.has_resources.load(Ordering::SeqCst)
    }

    pub fn resources(&self) -> Vec<Resource> {
        lock(&self.resources).clone()
    }

    pub fn resource_templates(&self) -> Vec<ResourceTemplate> {
        lock(&self.resource_templates).clone()
    }

    pub fn instructions(&self) -> Option<String> {
        lock(&self.instructions).clone()
    }

    pub fn challenge(&self) -> Option<OAuthChallenge> {
        lock(&self.challenge).clone()
    }

    pub fn set_challenge(&self, challenge: Option<OAuthChallenge>) {
        *lock(&self.challenge) = challenge;
    }

    /// `timeoutMs` (runtime.ts:206).
    pub fn timeout_ms(&self) -> u64 {
        self.entry
            .config
            .common()
            .timeout
            .filter(|timeout| *timeout > 0.0)
            .map(|timeout| (timeout * 1000.0) as u64)
            .unwrap_or((DEFAULT_TIMEOUT_SECONDS * 1000.0) as u64)
    }

    /// `oauthUrl` (runtime.ts:211): the server URL when OAuth is used.
    pub fn oauth_url(&self) -> Option<String> {
        if uses_oauth(&self.entry) {
            self.entry.config.url().map(str::to_owned)
        } else {
            None
        }
    }

    /// `oauthSettings` (runtime.ts:216).
    pub fn oauth_settings(&self) -> McpOAuthSettings {
        let mut settings = McpOAuthSettings::from_config(entry_oauth(&self.entry));
        if let Some(secret) = settings.client_secret.take() {
            settings.client_secret = rpi_ai::auth::config_value::resolve_config_value_or_throw(
                &secret,
                &format!("MCP server \"{}\" oauth.clientSecret", self.entry.name),
                None,
            )
            .ok();
        }
        settings
    }

    /// `getClient` (runtime.ts:229): opens on demand, sharing one open with
    /// concurrent callers.
    pub async fn get_client(self: &Arc<Self>) -> Result<Arc<McpClient>, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::Transport(format!(
                "MCP server \"{}\" is shut down",
                self.entry.name
            )));
        }
        if let Some(client) = self.client.lock().await.clone()
            && client.connection_state() == ClientState::Connected
        {
            return Ok(client);
        }
        let _guard = self.opening.lock().await;
        if let Some(client) = self.client.lock().await.clone()
            && client.connection_state() == ClientState::Connected
        {
            return Ok(client);
        }
        self.open().await
    }

    /// `withClient` (runtime.ts:277).
    async fn with_client<T, F, Fut>(
        self: &Arc<Self>,
        run: F,
        read_only: bool,
    ) -> Result<T, McpError>
    where
        F: Fn(Arc<McpClient>) -> Fut,
        Fut: Future<Output = Result<T, McpError>>,
    {
        let mut attempt = 1;
        loop {
            let client = self.get_client().await?;
            match run(client.clone()).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if read_only && attempt == 1 && error.is_transient() {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            CONNECT_RETRY_DELAYS_MS[0],
                        ))
                        .await;
                        attempt += 1;
                        continue;
                    }
                    if matches!(error, McpError::SessionExpired) && attempt == 1 {
                        // The server no longer knows the session, so it did
                        // not run the request; retry once on a new session.
                        // Closing the old client releases its GET stream task
                        // (v0.1.6 review P2-4; the slot reset alone leaked
                        // it).
                        self.drop_client(&client).await;
                        attempt += 1;
                        continue;
                    }
                    if self.needs_sign_in(&error) {
                        self.drop_client(&client).await;
                        self.mark_needs_auth();
                        return Err(McpError::Transport(sign_in_required_message(&self.entry)));
                    }
                    return Err(error);
                }
            }
        }
    }

    /// `reconnect` (runtime.ts:296): connect again with fresh credentials.
    pub async fn reconnect(self: &Arc<Self>) -> Result<(), McpError> {
        let _guard = self.opening.lock().await;
        if let Some(client) = self.client.lock().await.clone() {
            self.drop_client(&client).await;
        }
        self.open().await.map(|_| ())
    }

    /// `signOut` (runtime.ts:303): disconnect after the stored credentials
    /// were removed.
    pub async fn sign_out(self: &Arc<Self>) {
        let _guard = self.opening.lock().await;
        if let Some(client) = self.client.lock().await.clone() {
            self.drop_client(&client).await;
        }
        if !self.closed.load(Ordering::SeqCst) {
            self.mark_needs_auth();
        }
    }

    /// `needsSignIn` (runtime.ts:398).
    fn needs_sign_in(&self, error: &McpError) -> bool {
        error.is_authorization_required()
            || (self.auth_provider.is_some() && matches!(error, McpError::AuthRequired))
    }

    fn mark_needs_auth(&self) {
        *lock(&self.state) = ServerState::NeedsAuth;
        *lock(&self.error) = None;
        self.changed();
    }

    fn changed(&self) {
        if let Some(on_change) = &self.on_change {
            on_change(self);
        }
    }

    async fn drop_client(&self, client: &Arc<McpClient>) {
        let mut slot = self.client.lock().await;
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, client))
        {
            *slot = None;
        }
        drop(slot);
        let _ = client.close().await;
    }

    /// `open` (runtime.ts:420): connect with transient retries for HTTP.
    async fn open(self: &Arc<Self>) -> Result<Arc<McpClient>, McpError> {
        *lock(&self.state) = ServerState::Connecting;
        self.changed();
        let retries: &[u64] = if self.entry.config.url().is_some() {
            &CONNECT_RETRY_DELAYS_MS
        } else {
            &[]
        };
        let mut attempt = 0;
        loop {
            *lock(&self.stderr_tail) = None;
            match self.connect_once().await {
                Ok(client) => return Ok(client),
                Err(error) => {
                    let Some(delay) = retries.get(attempt) else {
                        return Err(self.connect_failed(error));
                    };
                    if self.closed.load(Ordering::SeqCst) || !error.is_transient() {
                        return Err(self.connect_failed(error));
                    }
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(*delay)).await;
                    if self.closed.load(Ordering::SeqCst) {
                        return Err(self.connect_failed(error));
                    }
                }
            }
        }
    }

    fn transport_auth(&self) -> Option<Arc<dyn AuthProvider>> {
        if let Some(provider) = &self.auth_provider {
            return Some(provider.clone());
        }
        self.provider_auth.clone()
    }

    /// `connectOnce` (runtime.ts:442).
    async fn connect_once(self: &Arc<Self>) -> Result<Arc<McpClient>, McpError> {
        let transport = (self.create_transport)(&self.entry, &self.cwd, self.transport_auth())
            .map_err(McpError::Transport)?;
        let mut options = McpClientOptions::new("rpi", crate::config::VERSION);
        options.request_timeout_ms = Some(self.timeout_ms());
        options.roots = vec![Root {
            uri: path_to_file_url(&self.cwd),
            name: Path::new(&self.cwd)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
        }];
        let client = McpClient::new(options);
        if let Some(log) = &self.log {
            let log = log.clone();
            let name = self.entry.name.clone();
            client.on_notification(
                "notifications/message",
                Arc::new(move |params| log.write(&name, params)),
            );
        }
        let connect = client.connect(transport.clone()).await;
        if let Err(error) = connect {
            self.capture_stderr(&transport);
            return Err(match error {
                McpError::AuthRequired => {
                    McpError::Transport(sign_in_required_message(&self.entry))
                }
                error => error,
            });
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let connection = self.clone();
            let client_for_refresh = client.clone();
            client.on_notification(
                "notifications/tools/list_changed",
                Arc::new(move |_| {
                    let connection = connection.clone();
                    let client = client_for_refresh.clone();
                    tokio::spawn(async move {
                        connection.refresh_tools(client, generation).await;
                    });
                }),
            );
        }
        {
            let connection = self.clone();
            let client_for_refresh = client.clone();
            client.on_notification(
                "notifications/resources/list_changed",
                Arc::new(move |_| {
                    let connection = connection.clone();
                    let client = client_for_refresh.clone();
                    tokio::spawn(async move {
                        connection.refresh_resources(client, generation).await;
                    });
                }),
            );
        }
        {
            let connection = self.clone();
            let client_for_close = client.clone();
            client.on_close(Arc::new(move || {
                connection.handle_client_close(&client_for_close);
            }));
        }
        let capabilities = client.server_capabilities();
        let has_resources = capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.resources.as_ref())
            .is_some();
        let tools = if capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.tools.as_ref())
            .is_some()
        {
            client.list_tools(McpRequestOptions::default()).await?
        } else {
            Vec::new()
        };
        let (resources, resource_templates) = if has_resources {
            fetch_resources(&client).await
        } else {
            (Vec::new(), Vec::new())
        };
        if self.closed.load(Ordering::SeqCst) {
            let _ = client.close().await;
            return Err(McpError::Transport("shut down while connecting".to_owned()));
        }
        *self.client.lock().await = Some(client.clone());
        *lock(&self.tools) = tools;
        self.has_resources.store(has_resources, Ordering::SeqCst);
        *lock(&self.resources) = resources;
        *lock(&self.resource_templates) = resource_templates;
        *lock(&self.instructions) = client
            .instructions()
            .map(|instructions| instructions.trim().to_owned())
            .filter(|instructions| !instructions.is_empty());
        *lock(&self.state) = ServerState::Connected;
        *lock(&self.error) = None;
        (self.on_tools)(self);
        self.changed();
        Ok(client)
    }

    /// `refreshTools` (runtime.ts:517).
    async fn refresh_tools(self: &Arc<Self>, client: Arc<McpClient>, generation: u64) {
        let result = client.list_tools(McpRequestOptions::default()).await;
        let same_client = self
            .client
            .lock()
            .await
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &client));
        if !same_client
            || self.closed.load(Ordering::SeqCst)
            || generation != self.generation.load(Ordering::SeqCst)
        {
            return;
        }
        match result {
            Ok(tools) => {
                *lock(&self.tools) = tools;
                (self.on_tools)(self);
            }
            Err(error) => {
                *lock(&self.error) = Some(format!("Failed to refresh tools: {error}"));
            }
        }
        self.changed();
    }

    /// `refreshResources` (runtime.ts:528).
    async fn refresh_resources(self: &Arc<Self>, client: Arc<McpClient>, generation: u64) {
        let (resources, templates) = fetch_resources(&client).await;
        let same_client = self
            .client
            .lock()
            .await
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &client));
        if !same_client
            || self.closed.load(Ordering::SeqCst)
            || generation != self.generation.load(Ordering::SeqCst)
        {
            return;
        }
        *lock(&self.resources) = resources;
        *lock(&self.resource_templates) = templates;
        (self.on_tools)(self);
        self.changed();
    }

    fn capture_stderr(&self, transport: &Arc<dyn McpTransport>) {
        if let Some(tail) = transport.stderr_tail() {
            let trimmed = tail.trim();
            if !trimmed.is_empty() {
                let tail = trimmed.to_string();
                let start = tail.chars().count().saturating_sub(STDERR_TAIL_CHARS);
                let tail: String = tail.chars().skip(start).collect();
                *lock(&self.stderr_tail) = Some(tail);
            }
        }
    }

    /// `connectFailed` (runtime.ts:490).
    fn connect_failed(&self, error: McpError) -> McpError {
        if self.needs_sign_in_error(&error) && !self.closed.load(Ordering::SeqCst) {
            self.mark_needs_auth();
            return McpError::Transport(sign_in_required_message(&self.entry));
        }
        let state = if self.closed.load(Ordering::SeqCst) {
            ServerState::Closed
        } else {
            ServerState::Failed
        };
        *lock(&self.state) = state;
        let message = match lock(&self.stderr_tail).clone() {
            Some(stderr) => format!("{error}\n{stderr}"),
            None => error.to_string(),
        };
        *lock(&self.error) = Some(message.clone());
        self.changed();
        McpError::Transport(format!(
            "MCP server \"{}\" failed to connect: {message}",
            self.entry.name
        ))
    }

    fn needs_sign_in_error(&self, error: &McpError) -> bool {
        match error {
            McpError::AuthorizationRequired => true,
            McpError::Transport(message) => {
                self.auth_provider.is_some() && message.contains("requires sign-in")
            }
            _ => false,
        }
    }

    /// `handleClientClose` (runtime.ts:501): the transport dropped; the next
    /// call reconnects.
    fn handle_client_close(self: &Arc<Self>, client: &Arc<McpClient>) {
        let connection = self.clone();
        let client = client.clone();
        tokio::spawn(async move {
            if connection.closed.load(Ordering::SeqCst) {
                return;
            }
            let mut guard = connection.client.lock().await;
            if !guard
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &client))
            {
                return;
            }
            *guard = None;
            drop(guard);
            *lock(&connection.state) = ServerState::Disconnected;
            let stderr = lock(&connection.stderr_tail).clone();
            *lock(&connection.error) = Some(match stderr {
                Some(stderr) => format!("Connection closed\n{stderr}"),
                None => "Connection closed".to_owned(),
            });
            connection.changed();
        });
    }

    /// `close` (runtime.ts:528).
    pub async fn close(self: &Arc<Self>) {
        self.closed.store(true, Ordering::SeqCst);
        *lock(&self.state) = ServerState::Closed;
        self.changed();
        let client = self.client.lock().await.take();
        if let Some(client) = client {
            let _ = client.close().await;
        }
        // A refresh the server already answered may have rotated the refresh
        // token; exiting before the new tokens are saved would lose the
        // grant.
        if let Some(provider) = &self.auth_provider {
            provider.settled().await;
        }
    }
}

/// `fetchResources` (runtime.ts:130): resources and templates at connect
/// time, without MCP App resources.
async fn fetch_resources(client: &Arc<McpClient>) -> (Vec<Resource>, Vec<ResourceTemplate>) {
    let resources = client
        .list_resources(McpRequestOptions::default())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|resource| {
            !super::resources::is_mcp_app_uri(&resource.uri, resource.mime_type.as_deref())
        })
        .collect();
    let templates = without_templates(
        client.list_resource_templates(McpRequestOptions::default()),
        Vec::new(),
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .filter(|template| {
        !super::resources::is_mcp_app_uri(&template.uri_template, template.mime_type.as_deref())
    })
    .collect();
    (resources, templates)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn path_to_file_url(path: &str) -> String {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    format!("file://{}", absolute.display())
}

/// `Arc<McpServerConnection>` tool caller handed to the tool definitions
/// (the connection methods need the `Arc` for lazy reconnects).
pub struct McpServerConnectionHandle(pub Arc<McpServerConnection>);

#[async_trait::async_trait]
impl McpToolCaller for McpServerConnectionHandle {
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
        options: McpCallOptions,
    ) -> Result<CallToolResult, McpError> {
        let connection = self.0.clone();
        let name = name.to_owned();
        let args = if args.is_null() {
            serde_json::json!({})
        } else {
            args
        };
        connection
            .with_client(
                move |client| {
                    let name = name.clone();
                    let args = args.clone();
                    let options = options.clone();
                    async move { client.call_tool(&name, Some(args), options.into()).await }
                },
                false,
            )
            .await
    }
}

/// `McpResourceServer` for the connection (runtime.ts:241-270).
#[async_trait::async_trait]
impl McpResourceServer for McpServerConnectionHandle {
    fn resource_server_name(&self) -> &str {
        self.0.name()
    }

    fn resource_server_timeout_ms(&self) -> u64 {
        self.0.timeout_ms()
    }

    async fn resources_page(
        &self,
        cursor: Option<String>,
        options: McpRequestOptions,
    ) -> Result<ListPage, McpError> {
        let connection = self.0.clone();
        connection
            .with_client(
                move |client| {
                    let cursor = cursor.clone();
                    let options = options.clone();
                    async move {
                        client
                            .list_resources_page(cursor, options)
                            .await
                            .map(page_from_resources)
                    }
                },
                true,
            )
            .await
    }

    async fn resource_templates_page(
        &self,
        cursor: Option<String>,
        options: McpRequestOptions,
    ) -> Result<ListPage, McpError> {
        let connection = self.0.clone();
        connection
            .with_client(
                move |client| {
                    let cursor = cursor.clone();
                    let options = options.clone();
                    async move {
                        without_templates(
                            async {
                                client
                                    .list_resource_templates_page(cursor, options)
                                    .await
                                    .map(page_from_templates)
                            },
                            ListPage {
                                items: Vec::new(),
                                next_cursor: None,
                            },
                        )
                        .await
                    }
                },
                true,
            )
            .await
    }

    async fn all_resources(
        &self,
        options: McpRequestOptions,
    ) -> Result<Vec<serde_json::Value>, McpError> {
        let connection = self.0.clone();
        connection
            .with_client(
                move |client| {
                    let options = options.clone();
                    async move {
                        Ok(client
                            .list_resources(options)
                            .await?
                            .into_iter()
                            .map(|resource| {
                                serde_json::to_value(resource).unwrap_or(serde_json::Value::Null)
                            })
                            .collect())
                    }
                },
                true,
            )
            .await
    }

    async fn all_resource_templates(
        &self,
        options: McpRequestOptions,
    ) -> Result<Vec<serde_json::Value>, McpError> {
        let connection = self.0.clone();
        connection
            .with_client(
                move |client| {
                    let options = options.clone();
                    async move {
                        without_templates(
                            async {
                                client
                                    .list_resource_templates(options)
                                    .await
                                    .map(|templates| {
                                        templates
                                            .into_iter()
                                            .map(|template| {
                                                serde_json::to_value(template)
                                                    .unwrap_or(serde_json::Value::Null)
                                            })
                                            .collect()
                                    })
                            },
                            Vec::new(),
                        )
                        .await
                    }
                },
                true,
            )
            .await
    }

    async fn read_resource(
        &self,
        uri: &str,
        options: McpRequestOptions,
    ) -> Result<serde_json::Value, McpError> {
        let connection = self.0.clone();
        let uri = uri.to_owned();
        connection
            .with_client(
                move |client| {
                    let uri = uri.clone();
                    let options = options.clone();
                    async move { client.read_resource(&uri, options).await }
                },
                true,
            )
            .await
    }
}

fn page_from_resources((resources, next_cursor): (Vec<Resource>, Option<String>)) -> ListPage {
    ListPage {
        items: resources
            .into_iter()
            .map(|resource| serde_json::to_value(resource).unwrap_or(serde_json::Value::Null))
            .collect(),
        next_cursor,
    }
}

fn page_from_templates(
    (templates, next_cursor): (Vec<ResourceTemplate>, Option<String>),
) -> ListPage {
    ListPage {
        items: templates
            .into_iter()
            .map(|template| serde_json::to_value(template).unwrap_or(serde_json::Value::Null))
            .collect(),
        next_cursor,
    }
}

/// Conversion from the runtime's call options to the client's request
/// options.
impl From<McpCallOptions> for McpRequestOptions {
    fn from(options: McpCallOptions) -> Self {
        McpRequestOptions {
            signal: options.signal,
            timeout_ms: options.timeout_ms,
            on_progress: options.on_progress,
        }
    }
}

/// Parse a challenge header (used by `/mcp` refresh paths).
pub fn challenge_from_header(header: Option<&str>) -> OAuthChallenge {
    parse_www_authenticate(header)
}

/// `Weak` handle helper.
pub fn weak(connection: &Arc<McpServerConnection>) -> Weak<McpServerConnection> {
    Arc::downgrade(connection)
}
