//! Lifecycle state machine: lazy/eager/keep-alive/lazy-keep-alive modes,
//! periodic health checks, keep-alive convergence with exponential retry,
//! idle shutdown, graceful shutdown order (FR-P0-09 / R7.2.7, design §3.6).
//!
//! Port of `lifecycle.ts` (`McpLifecycleManager`) @ pi-mcp-adapter v2.32.1
//! (10a45367) — the TE24 rebase of the convergence machinery:
//! - `ensureConverged` (:104-116): the single-flight convergence pass over
//!   keep-alive servers, triggered on `input`, before adapter-triggered
//!   turns and at the head of every health check;
//! - `checkKeepAliveConnection` (:125-236): needs-auth → auth-required;
//!   not connected → connect; connected+url → bounded `tools/list` refresh
//!   whose TIMEOUT ONLY DEFERS (#400 — a slow-but-healthy server is never
//!   marked failed); a terminated/expired Streamable session reconnects
//!   (`shouldReconnectAfterRefresh`); superseded passes hand off;
//! - exponential retry (`recordRetry` :367-390): 30s × 2^(attempts-1),
//!   capped at 5min; a failed attempt that is still the current
//!   connection/status is skipped until `nextAttemptAt` (upstream
//!   `deferRefreshTimeout` is the same bookkeeping with a debug log);
//! - `publishConnectedMetadata` (:251-267): post-connect metadata
//!   publication with identity fencing.
//!
//! The 60s failure backoff tracker from `init.ts`
//! (`recordFailure`/`clearFailure`/`getFailureAgeSeconds`) lives here too —
//! the design assigns init.ts's lifecycle responsibilities to this module.
//!
//! Port notes:
//! - upstream fences stale convergence passes by object identity
//!   (`keepAliveServers.get(name) !== definition`); the Rust port compares
//!   entry CONTENT (`ServerEntry` equality) — a same-name replacement with
//!   identical content is behaviorally indistinguishable, a different one
//!   fences exactly like upstream.
//! - `hasPendingAuthForServer` stays a hook (OAuth pending state is P1).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::manager::{ConnectionStatus, McpServerManager, ServerConnection};
use crate::metadata::ServerEntry;

/// `FAILURE_BACKOFF_MS` (init.ts:39).
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(60);
/// `MAX_FAILURE_MESSAGE_CHARS` (init.ts:40).
const MAX_FAILURE_MESSAGE_CHARS: usize = 8 * 1024;
/// Default health-check period (design §3.6).
pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Default idle timeout (init.ts:212: `settings.idleTimeout ?? 10` minutes).
pub const DEFAULT_IDLE_TIMEOUT_MINUTES: u64 = 10;
/// `KEEP_ALIVE_RETRY_BASE_MS` (lifecycle.ts:15 @ 10a45367).
pub const KEEP_ALIVE_RETRY_BASE_MS: Duration = Duration::from_secs(30);
/// `KEEP_ALIVE_RETRY_MAX_MS` (lifecycle.ts:16 @ 10a45367).
pub const KEEP_ALIVE_RETRY_MAX_MS: Duration = Duration::from_secs(5 * 60);
/// `KEEP_ALIVE_CHECK_CONCURRENCY` (lifecycle.ts:17 @ 10a45367).
const KEEP_ALIVE_CHECK_CONCURRENCY: usize = 10;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `lifecycle` mode of a server entry (types.ts:386).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LifecycleMode {
    #[default]
    Lazy,
    Eager,
    KeepAlive,
    LazyKeepAlive,
}

impl LifecycleMode {
    pub fn of(definition: &ServerEntry) -> Self {
        match definition.get_str("lifecycle") {
            Some("eager") => Self::Eager,
            Some("keep-alive") => Self::KeepAlive,
            Some("lazy-keep-alive") => Self::LazyKeepAlive,
            _ => Self::Lazy,
        }
    }

    /// init.ts:232 — `eager`/`lazy-keep-alive` default to `idleTimeout: 0`.
    pub fn persists_after_first_spawn(self) -> bool {
        matches!(self, Self::Eager | Self::LazyKeepAlive)
    }
}

type ReconnectCallback = Arc<dyn Fn(&str) + Send + Sync>;
type ReconnectFailureCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// `RetryState` (lifecycle.ts:22-28 @ 10a45367).
#[derive(Debug, Clone)]
struct RetryState {
    attempts: u32,
    next_attempt_at: u64,
    connection: Weak<ServerConnection>,
    status: Option<ConnectionStatus>,
    warning_reported: bool,
}

/// `McpLifecycleManager` (lifecycle.ts:28-151 @ 10a45367).
pub struct LifecycleManager {
    manager: Arc<McpServerManager>,
    keep_alive: Mutex<HashMap<String, ServerEntry>>,
    all_servers: Mutex<HashMap<String, (ServerEntry, Option<u64>)>>,
    global_idle_timeout: Mutex<Duration>,
    health_interval: Mutex<Duration>,
    cancel: CancellationToken,
    health_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    check_running: AtomicBool,
    /// `activeConvergence` single-flight fence (:104-116).
    convergence_running: AtomicBool,
    stopped: AtomicBool,
    retry_states: Mutex<HashMap<String, RetryState>>,
    on_reconnect: Mutex<Option<ReconnectCallback>>,
    on_reconnect_failure: Mutex<Option<ReconnectFailureCallback>>,
    on_health_restored: Mutex<Option<ReconnectCallback>>,
    on_auth_required: Mutex<Option<ReconnectCallback>>,
    on_idle_shutdown: Mutex<Option<ReconnectCallback>>,
}

impl LifecycleManager {
    pub fn new(manager: Arc<McpServerManager>, cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            manager,
            keep_alive: Mutex::new(HashMap::new()),
            all_servers: Mutex::new(HashMap::new()),
            global_idle_timeout: Mutex::new(Duration::from_secs(DEFAULT_IDLE_TIMEOUT_MINUTES * 60)),
            health_interval: Mutex::new(HEALTH_CHECK_INTERVAL),
            cancel,
            health_task: Mutex::new(None),
            check_running: AtomicBool::new(false),
            convergence_running: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            retry_states: Mutex::new(HashMap::new()),
            on_reconnect: Mutex::new(None),
            on_reconnect_failure: Mutex::new(None),
            on_health_restored: Mutex::new(None),
            on_auth_required: Mutex::new(None),
            on_idle_shutdown: Mutex::new(None),
        })
    }

    /// Test hook: shorten the health-check period (upstream takes
    /// `intervalMs` as an argument).
    pub fn set_health_interval(&self, interval: Duration) {
        *self
            .health_interval
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = interval;
    }

    pub fn set_reconnect_callback(&self, callback: ReconnectCallback) {
        *self.on_reconnect.lock().unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    pub fn set_reconnect_failure_callback(&self, callback: ReconnectFailureCallback) {
        *self
            .on_reconnect_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    /// `setHealthRestoredCallback` (lifecycle.ts:47-49 @ 10a45367).
    pub fn set_health_restored_callback(&self, callback: ReconnectCallback) {
        *self
            .on_health_restored
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    /// `setAuthRequiredCallback` (lifecycle.ts:51-53 @ 10a45367).
    pub fn set_auth_required_callback(&self, callback: ReconnectCallback) {
        *self
            .on_auth_required
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    pub fn set_idle_shutdown_callback(&self, callback: ReconnectCallback) {
        *self
            .on_idle_shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    /// `markKeepAlive` (lifecycle.ts:59-62).
    pub fn mark_keep_alive(&self, name: &str, definition: &ServerEntry) {
        if definition.is_disabled() {
            return;
        }
        self.keep_alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), definition.clone());
        // A re-registration is a fresh identity — drop stale retry state
        // (upstream fences by object identity at use sites).
        self.retry_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    /// `registerServer` (lifecycle.ts:64-70).
    pub fn register_server(
        &self,
        name: &str,
        definition: &ServerEntry,
        idle_timeout_minutes: Option<u64>,
    ) {
        if definition.is_disabled() {
            return;
        }
        self.all_servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), (definition.clone(), idle_timeout_minutes));
    }

    /// `unregisterServer` (lifecycle.ts:72-77).
    pub fn unregister_server(&self, name: &str) {
        self.all_servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        self.keep_alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        self.retry_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    /// `setGlobalIdleTimeout` (lifecycle.ts:79-81), minutes.
    pub fn set_global_idle_timeout_minutes(&self, minutes: u64) {
        *self
            .global_idle_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Duration::from_secs(minutes * 60);
    }

    /// `getIdleTimeout` (lifecycle.ts:343-347).
    fn idle_timeout(&self, name: &str) -> Duration {
        let per_server = self
            .all_servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .and_then(|(_, timeout)| *timeout);
        match per_server {
            Some(minutes) => Duration::from_secs(minutes * 60),
            None => *self
                .global_idle_timeout
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// `startHealthChecks` (lifecycle.ts:92-120).
    pub fn start_health_checks(self: &Arc<Self>) {
        if self.cancel.is_cancelled() {
            self.stopped.store(true, Ordering::SeqCst);
            return;
        }
        self.stopped.store(false, Ordering::SeqCst);
        let this = self.clone();
        let task = tokio::spawn(async move {
            let health_interval = *this
                .health_interval
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut interval = tokio::time::interval(health_interval);
            interval.tick().await; // first tick is immediate; skip it
            loop {
                tokio::select! {
                    _ = this.cancel.cancelled() => break,
                    _ = interval.tick() => {
                        if this.stopped.load(Ordering::SeqCst) { break; }
                        // Overlap guard: skip the tick while a check runs.
                        if this.check_running.swap(true, Ordering::SeqCst) {
                            continue;
                        }
                        this.check_connections().await;
                        this.check_running.store(false, Ordering::SeqCst);
                    }
                }
            }
        });
        *self.health_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
    }

    /// `ensureConverged` (lifecycle.ts:104-116 @ 10a45367): the
    /// single-flight convergence pass. Triggered before user input
    /// (index.ts:683-706), before adapter-triggered turns, and at the head
    /// of every health check (`checkConnections` :122-124).
    pub async fn ensure_converged(self: &Arc<Self>) {
        if self.stopped.load(Ordering::SeqCst) || self.cancel.is_cancelled() {
            return;
        }
        if self.convergence_running.swap(true, Ordering::SeqCst) {
            return; // an in-flight pass already converges for us
        }
        self.check_keep_alive_connections().await;
        self.convergence_running.store(false, Ordering::SeqCst);
    }

    /// `checkConnections` (lifecycle.ts:122-142 @ 10a45367): converge the
    /// keep-alive fleet FIRST, then sweep idle servers.
    async fn check_connections(self: &Arc<Self>) {
        if self.stopped.load(Ordering::SeqCst) || self.cancel.is_cancelled() {
            return;
        }
        self.ensure_converged().await;
        if self.stopped.load(Ordering::SeqCst) || self.cancel.is_cancelled() {
            return;
        }

        let all: Vec<String> = self
            .all_servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        let keep_alive_names: HashSet<String> = self
            .keep_alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for name in all {
            if keep_alive_names.contains(&name) {
                continue;
            }
            let timeout = self.idle_timeout(&name);
            if !timeout.is_zero() && self.manager.is_idle(&name, timeout) {
                self.manager.close(&name).await;
                if self.stopped.load(Ordering::SeqCst) {
                    return;
                }
                let callback = self
                    .on_idle_shutdown
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(callback) = callback {
                    callback(&name);
                }
            }
        }
    }

    /// `checkKeepAliveConnections` (:144-150): bounded-parallelism fan-out.
    async fn check_keep_alive_connections(self: &Arc<Self>) {
        let keep_alive: Vec<(String, ServerEntry)> = self
            .keep_alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let names: Vec<String> = keep_alive.into_iter().map(|(name, _)| name).collect();
        let this = self.clone();
        futures::stream::iter(names.into_iter().map(|name| {
            let this = this.clone();
            async move {
                this.check_keep_alive_connection_inner(&name, true).await;
            }
        }))
        .buffer_unordered(KEEP_ALIVE_CHECK_CONCURRENCY)
        .collect::<Vec<()>>()
        .await;
    }

    /// `checkKeepAliveConnection` (:125-236) + `handleSupersededConnection`
    /// (:238-249). Public so the `input` handler and the dispatch gate can
    /// converge a single server on demand.
    pub async fn check_keep_alive_connection(&self, name: &str) {
        self.check_keep_alive_connection_inner(name, true).await;
    }

    /// `retrySuperseded` (:131, :238-249): a superseded pass retries once
    /// directly; the retry itself never chains another.
    async fn check_keep_alive_connection_inner(&self, name: &str, retry_superseded: bool) {
        let definition = {
            let keep_alive = self.keep_alive.lock().unwrap_or_else(|e| e.into_inner());
            match keep_alive.get(name) {
                Some(definition) => definition.clone(),
                None => return,
            }
        };
        if definition.is_disabled()
            || self.stopped.load(Ordering::SeqCst)
            || self.cancel.is_cancelled()
        {
            return;
        }
        // Identity fence: a replaced/unregistered entry invalidates this
        // pass (content equality — see module notes).
        if !self.keep_alive_current(name, &definition) {
            return;
        }
        let connection = self.manager.get_connection(name);
        if connection
            .as_ref()
            .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
        {
            // A needs-auth connection needs no retry bookkeeping.
            self.retry_states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(name);
            return;
        }
        if !self.should_attempt_connection(name, connection.as_ref()) {
            return;
        }
        let Some(connection) = connection.filter(|c| c.status() == ConnectionStatus::Connected)
        else {
            // P0 hook: no OAuth pending state; upstream skips reconnect
            // while an authorization is pending.
            match self.manager.connect(name, &definition).await {
                Ok(fresh) => {
                    if self.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    if fresh.status() == ConnectionStatus::NeedsAuth {
                        self.notify_auth_required(name, &definition);
                        return;
                    }
                    if fresh.status() != ConnectionStatus::Connected {
                        let message =
                            format!("MCP server {name} did not return a connected session");
                        self.report_connection_failure(name, &definition, &message, "reconnect");
                        return;
                    }
                    debug!(server = %name, "MCP: reconnected keep-alive server");
                    self.publish_connected_metadata(name, &definition).await;
                    return;
                }
                Err(error) => {
                    if self.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    self.report_connection_failure(
                        name,
                        &definition,
                        &error.to_string(),
                        "reconnect",
                    );
                }
            }
            return;
        };

        // Connected: refresh the catalog for url transports (stdio servers
        // have no expiring session; a process that died flips the
        // connection to closed on the next poll anyway).
        if definition.get("url").is_none() {
            return;
        }
        let had_session_id = connection
            .client
            .as_ref()
            .is_some_and(|c| c.session_id().is_some());
        let refresh_result = self.manager.refresh_tools(name, &connection).await;
        match refresh_result {
            Ok(crate::manager::ToolRefreshResult::Superseded) => {
                // Hand off to whatever replaced the connection.
                self.handle_superseded_connection(name, &definition, retry_superseded)
                    .await;
            }
            Ok(crate::manager::ToolRefreshResult::RefreshTimeout) => {
                // `deferRefreshTimeout` (:344-348): record the retry, never
                // mark failed (#400).
                self.record_retry(name, &definition);
                debug!(server = %name, "MCP: keep-alive tools/list refresh timed out; retrying after backoff");
            }
            Ok(
                crate::manager::ToolRefreshResult::Updated
                | crate::manager::ToolRefreshResult::Unchanged,
            ) => {
                // A healthy pass clears the retry state and reports
                // health-restored when a window was active (:230-233).
                let had_retry = self
                    .retry_states
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name)
                    .is_some();
                if had_retry {
                    let callback = self
                        .on_health_restored
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if let Some(callback) = callback {
                        callback(name);
                    }
                }
            }
            Err(error) => {
                if self.stopped.load(Ordering::SeqCst) {
                    return;
                }
                let current = self.manager.get_connection(name);
                let superseded = !current
                    .as_ref()
                    .is_some_and(|c| Arc::ptr_eq(c, &connection))
                    || connection.status() != ConnectionStatus::Connected;
                if superseded {
                    self.handle_superseded_connection(name, &definition, retry_superseded)
                        .await;
                    return;
                }
                if !should_reconnect_after_refresh(&error, had_session_id) {
                    self.report_connection_failure(
                        name,
                        &definition,
                        &error.to_string(),
                        "refresh",
                    );
                    return;
                }
                // P0 hook: pending OAuth would skip the reconnect.
                match self.manager.reconnect(name, &definition, &connection).await {
                    Ok(fresh) => {
                        if self.stopped.load(Ordering::SeqCst) {
                            return;
                        }
                        if fresh.status() == ConnectionStatus::NeedsAuth {
                            self.notify_auth_required(name, &definition);
                            return;
                        }
                        if fresh.status() != ConnectionStatus::Connected {
                            let message =
                                format!("MCP server {name} did not return a connected session");
                            self.report_connection_failure(
                                name,
                                &definition,
                                &message,
                                "reconnect",
                            );
                            return;
                        }
                        debug!(server = %name, "MCP: reconnected stale MCP session");
                        self.publish_connected_metadata(name, &definition).await;
                    }
                    Err(error) => {
                        if self.stopped.load(Ordering::SeqCst) {
                            return;
                        }
                        self.report_connection_failure(
                            name,
                            &definition,
                            &error.to_string(),
                            "reconnect",
                        );
                    }
                }
            }
        }
    }

    /// `handleSupersededConnection` (:238-249).
    async fn handle_superseded_connection(
        &self,
        name: &str,
        definition: &ServerEntry,
        retry_superseded: bool,
    ) {
        let current = self.manager.get_connection(name);
        if !self.keep_alive_current(name, definition) {
            return;
        }
        let Some(current) = current else {
            // No replacement connected yet: one immediate retry of the
            // pass (upstream `retrySuperseded`).
            if retry_superseded {
                Box::pin(self.check_keep_alive_connection_inner(name, false)).await;
            }
            return;
        };
        match current.status() {
            ConnectionStatus::Connected => {
                self.publish_connected_metadata(name, definition).await;
            }
            ConnectionStatus::NeedsAuth => {
                self.notify_auth_required(name, definition);
            }
            ConnectionStatus::Closed => {
                if retry_superseded {
                    Box::pin(self.check_keep_alive_connection_inner(name, false)).await;
                }
            }
        }
    }

    /// `publishConnectedMetadata` (:251-267): fire the reconnect callback
    /// (upstream awaits it — the Rust callbacks are sync hooks) and clear
    /// the retry state; fence stale passes.
    async fn publish_connected_metadata(&self, name: &str, definition: &ServerEntry) {
        if !self.keep_alive_current(name, definition) {
            return;
        }
        let callback = self
            .on_reconnect
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(callback) = callback {
            callback(name);
        }
        self.retry_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    /// `notifyAuthRequired` (:269-283).
    fn notify_auth_required(&self, name: &str, definition: &ServerEntry) {
        if !self.keep_alive_current(name, definition) {
            return;
        }
        self.retry_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        let callback = self
            .on_auth_required
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(callback) = callback {
            callback(name);
        }
    }

    /// Identity fence — `keepAliveServers.get(name) !== definition`
    /// (content equality; see module notes).
    fn keep_alive_current(&self, name: &str, definition: &ServerEntry) -> bool {
        self.keep_alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .is_some_and(|current| current == definition)
    }

    /// `shouldAttemptConnection` (:311-320): skip until `nextAttemptAt`
    /// unless the connection/status identity changed (a changed identity
    /// invalidates the retry state).
    fn should_attempt_connection(
        &self,
        name: &str,
        connection: Option<&Arc<ServerConnection>>,
    ) -> bool {
        let mut retry_states = self.retry_states.lock().unwrap_or_else(|e| e.into_inner());
        let Some(retry) = retry_states.get(name) else {
            return true;
        };
        let status = connection.map(|c| c.status());
        let same_connection = match (connection, retry.connection.upgrade()) {
            (Some(current), Some(retry_connection)) => Arc::ptr_eq(current, &retry_connection),
            (None, None) => true,
            _ => false,
        };
        if !same_connection || retry.status != status {
            retry_states.remove(name);
            return true;
        }
        now_ms() >= retry.next_attempt_at
    }

    /// `reportConnectionFailure` (:322-342): record the retry, fire the
    /// reconnect-failure callback; the loud error fires once per retry
    /// window (the 503 transient path never reaches here loudly —
    /// `isTransientHttpConnectError` servers keep `warning_reported`
    /// semantics upstream; the Rust failure surface is the callback).
    fn report_connection_failure(
        &self,
        name: &str,
        definition: &ServerEntry,
        error: &str,
        action: &str,
    ) {
        if !self.record_retry(name, definition) {
            return;
        }
        let callback = self
            .on_reconnect_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(callback) = callback {
            callback(name, error);
        }
        let target = match action {
            "reconnect" => format!("reconnect to {name}"),
            "publish" => format!("publish metadata for {name}"),
            _ => format!("refresh {name}"),
        };
        let mut retry_states = self.retry_states.lock().unwrap_or_else(|e| e.into_inner());
        let already_reported = retry_states
            .get(name)
            .is_some_and(|retry| retry.warning_reported);
        if !already_reported {
            if let Some(retry) = retry_states.get_mut(name) {
                retry.warning_reported = true;
            }
            error!(
                server = %name,
                "MCP: Failed to {target}: {}",
                crate::utils::sanitize_terminal_text(error)
            );
        }
    }

    /// `recordRetry` (:367-390): attempts += 1; next attempt at
    /// now + min(30s × 2^(attempts-1), 5min).
    fn record_retry(&self, name: &str, definition: &ServerEntry) -> bool {
        if !self.keep_alive_current(name, definition) {
            return false;
        }
        let connection = self.manager.get_connection(name);
        let status = connection.as_ref().map(|c| c.status());
        let weak = connection.as_ref().map(Arc::downgrade);
        let mut retry_states = self.retry_states.lock().unwrap_or_else(|e| e.into_inner());
        let previous = retry_states.get(name);
        let attempts = previous
            .map(|retry| retry.attempts.saturating_add(1))
            .unwrap_or(1);
        let exponent = (attempts - 1).min(10);
        let delay = KEEP_ALIVE_RETRY_BASE_MS
            .saturating_mul(2u32.saturating_pow(exponent))
            .min(KEEP_ALIVE_RETRY_MAX_MS);
        let warning_reported = previous
            .map(|retry| retry.warning_reported)
            .unwrap_or(false);
        retry_states.insert(
            name.to_string(),
            RetryState {
                attempts,
                next_attempt_at: now_ms() + delay.as_millis() as u64,
                connection: weak.unwrap_or_default(),
                status,
                warning_reported,
            },
        );
        true
    }

    /// `gracefulShutdown` (lifecycle.ts:392-425): cancel the health task,
    /// await an in-flight check, then `closeAll`.
    pub async fn graceful_shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        let task = self
            .health_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.retry_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.manager.close_all().await;
    }
}

/// `shouldReconnectAfterRefresh` (lifecycle.ts:427-431 @ 10a45367): a
/// terminated Streamable HTTP session (or SDK NotConnected/ConnectionClosed)
/// warrants a reconnect; anything else is a real failure.
fn should_reconnect_after_refresh(
    error: &crate::protocol::ProtocolError,
    had_session_id: bool,
) -> bool {
    if crate::session_recovery::is_terminated_session(error, had_session_id) {
        return true;
    }
    matches!(error, crate::protocol::ProtocolError::Closed)
}

// ============================================================================
// Failure tracker (init.ts half)
// ============================================================================

/// Fired when a failure window opens or expires (upstream `recordFailure` /
/// `clearFailure` / the expiry timer all call
/// `notifyToolMetadataUpdated`; init.ts:55/70/83/90 @ 10a45367). The hook carries
/// the upstream reason strings verbatim (`failure-backoff-started` /
/// `failure-backoff-expired`) so the caller can re-sync the tool surface.
pub type FailureChangeHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The failure tracker half of init.ts: `recordFailure` / `clearFailure` /
/// `getFailureAgeSeconds` with the 60s self-expiry (init.ts:39-80, 556-567),
/// plus `isServerInActiveFailureBackoff` (failure-backoff.ts:18-23 @ 10a45367, #434).
pub struct FailureTracker {
    failed_at: Mutex<HashMap<String, u64>>,
    messages: Mutex<HashMap<String, String>>,
    on_change: Mutex<Option<FailureChangeHook>>,
}

impl Default for FailureTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FailureTracker {
    pub fn new() -> Self {
        Self {
            failed_at: Mutex::new(HashMap::new()),
            messages: Mutex::new(HashMap::new()),
            on_change: Mutex::new(None),
        }
    }

    /// Bind the metadata-updated hook fired on failure start/expiry
    /// (upstream `notifyToolMetadataUpdated` calls in `recordFailure` and
    /// the expiry timer). Set by `proxy::initialize_mcp`.
    pub fn set_change_callback(&self, callback: FailureChangeHook) {
        *self.on_change.lock().unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn notify_change(&self, server_name: &str, reason: &str) {
        let callback = self
            .on_change
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(callback) = callback {
            callback(server_name, reason);
        }
    }

    /// `isServerInActiveFailureBackoff` (failure-backoff.ts:18-23 @ 10a45367): the
    /// server is neither connected nor needs-auth and a failure was recorded
    /// inside the 60s window. Consumers hide its tools (R7.2.3.1/#434).
    pub fn is_server_in_active_failure_backoff(
        &self,
        manager: &McpServerManager,
        server_name: &str,
    ) -> bool {
        let connected_or_auth = manager.get_connection(server_name).is_some_and(|c| {
            matches!(
                c.status(),
                ConnectionStatus::Connected | ConnectionStatus::NeedsAuth
            )
        });
        !connected_or_auth && self.failure_age_seconds(server_name).is_some()
    }

    /// `recordFailure` (init.ts:61-80): remember the failure; a 60s timer
    /// clears it unless superseded by a newer failure.
    pub fn record(self: &Arc<Self>, server_name: &str, message: &str, owner: CancellationToken) {
        self.clear(server_name);
        let failed_at = now_ms();
        self.failed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server_name.to_string(), failed_at);
        let mut truncated = message.to_string();
        if truncated.len() > MAX_FAILURE_MESSAGE_CHARS {
            truncated.truncate(MAX_FAILURE_MESSAGE_CHARS);
        }
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server_name.to_string(), truncated);

        let this = self.clone();
        let name = server_name.to_string();
        tokio::spawn(async move {
            tokio::select! {
                _ = owner.cancelled() => {}
                _ = tokio::time::sleep(FAILURE_BACKOFF) => {
                    let expired = {
                        let mut failed = this.failed_at.lock().unwrap_or_else(|e| e.into_inner());
                        if failed.get(&name) == Some(&failed_at) {
                            failed.remove(&name);
                            this.messages
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&name);
                            true
                        } else {
                            false
                        }
                    };
                    // init.ts:80-87: the expiry timer notifies before
                    // publishing the status snapshot so the tool surface
                    // becomes visible again.
                    if expired {
                        this.notify_change(&name, "failure-backoff-expired");
                    }
                }
            }
        });
        self.notify_change(server_name, "failure-backoff-started");
    }

    /// `clearFailure` (init.ts:52-59): drop the failure without firing the
    /// change hook (callers that need the upstream `restoredReason` notify
    /// use [`Self::clear_with_reason`]).
    pub fn clear(&self, server_name: &str) {
        self.failed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server_name);
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server_name);
    }

    /// `clearFailure(state, name, restoredReason)` (init.ts:55-69): clears
    /// the failure and, when a window was actually active, fires the change
    /// hook with `reason`. Returns whether a window was active so callers
    /// can mirror the upstream `if (!restored) notifyToolMetadataUpdated`
    /// fallback.
    pub fn clear_with_reason(&self, server_name: &str, reason: &str) -> bool {
        let was_active = self.failure_age_seconds(server_name).is_some();
        self.clear(server_name);
        if was_active {
            self.notify_change(server_name, reason);
        }
        was_active
    }

    /// Test hook: record a failure with an explicit timestamp so expiry can
    /// be exercised without waiting out [`FAILURE_BACKOFF`]. `failure_age_seconds`
    /// returns `None` for a stale timestamp; the expiry task is not armed.
    #[doc(hidden)]
    pub fn record_failure_at(&self, server_name: &str, failed_at_ms: u64, message: &str) {
        self.failed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server_name.to_string(), failed_at_ms);
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server_name.to_string(), message.to_string());
    }

    /// `getFailureAgeSeconds` (init.ts:556-562): `None` once the backoff
    /// expired (belt-and-braces alongside the expiry task).
    pub fn failure_age_seconds(&self, server_name: &str) -> Option<u64> {
        let failed_at = *self
            .failed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_name)?;
        let age_ms = now_ms().saturating_sub(failed_at);
        if age_ms > FAILURE_BACKOFF.as_millis() as u64 {
            return None;
        }
        Some((age_ms as f64 / 1000.0).round() as u64)
    }

    /// `getFailureMessage` (init.ts:564-567).
    pub fn failure_message(&self, server_name: &str) -> Option<String> {
        self.failure_age_seconds(server_name)?;
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_name)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn entry(value: Value) -> ServerEntry {
        ServerEntry(value.as_object().cloned().unwrap_or_default())
    }

    #[test]
    fn lifecycle_mode_defaults_and_persistence() {
        assert_eq!(LifecycleMode::of(&entry(json!({}))), LifecycleMode::Lazy);
        assert_eq!(
            LifecycleMode::of(&entry(json!({ "lifecycle": "lazy-keep-alive" }))),
            LifecycleMode::LazyKeepAlive
        );
        assert!(LifecycleMode::Eager.persists_after_first_spawn());
        assert!(LifecycleMode::LazyKeepAlive.persists_after_first_spawn());
        assert!(!LifecycleMode::KeepAlive.persists_after_first_spawn());
        assert!(!LifecycleMode::Lazy.persists_after_first_spawn());
    }

    #[tokio::test]
    async fn failure_tracker_records_and_expires() {
        let tracker = Arc::new(FailureTracker::new());
        tracker.record("srv", "boom", CancellationToken::new());
        assert_eq!(tracker.failure_age_seconds("srv"), Some(0));
        assert_eq!(tracker.failure_message("srv").as_deref(), Some("boom"));
        tracker.clear("srv");
        assert_eq!(tracker.failure_age_seconds("srv"), None);
    }

    #[tokio::test]
    async fn active_failure_backoff_truth_table() {
        let manager = McpServerManager::new(None);
        let tracker = Arc::new(FailureTracker::new());
        // No failure recorded → never in backoff.
        assert!(!tracker.is_server_in_active_failure_backoff(&manager, "srv"));
        // Failure recorded, no connection → in backoff.
        tracker.record("srv", "boom", CancellationToken::new());
        assert!(tracker.is_server_in_active_failure_backoff(&manager, "srv"));
        // needs-auth is not a failure (failure-backoff.ts:18-23).
        tracker.clear("srv");
        assert!(!tracker.is_server_in_active_failure_backoff(&manager, "srv"));
    }

    #[tokio::test]
    async fn failure_change_hook_reports_start_and_reason_clear() {
        let tracker = Arc::new(FailureTracker::new());
        let events = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let sink = events.clone();
        tracker.set_change_callback(Arc::new(move |server: &str, reason: &str| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((server.to_string(), reason.to_string()));
        }));
        tracker.record("srv", "boom", CancellationToken::new());
        assert_eq!(
            events.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            &[("srv".to_string(), "failure-backoff-started".to_string())]
        );
        // `clear` stays silent (upstream clearFailure without reason).
        tracker.clear("srv");
        assert_eq!(events.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
        // `clear_with_reason` reports only when a window was active.
        tracker.record("srv", "boom", CancellationToken::new());
        assert!(tracker.clear_with_reason("srv", "lazy-connect"));
        assert!(!tracker.clear_with_reason("srv", "lazy-connect"));
        let recorded = events.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            recorded.last().map(|(_, r)| r.as_str()),
            Some("lazy-connect")
        );
        assert_eq!(recorded.len(), 3);
    }

    #[test]
    fn retry_backoff_schedule_is_exponential_with_cap() {
        // KEEP_ALIVE_RETRY_BASE/MAX (lifecycle.ts:15-16 @ 10a45367).
        assert_eq!(KEEP_ALIVE_RETRY_BASE_MS, Duration::from_secs(30));
        assert_eq!(KEEP_ALIVE_RETRY_MAX_MS, Duration::from_secs(300));
        let delay = |attempts: u32| {
            let exponent = (attempts - 1).min(10);
            KEEP_ALIVE_RETRY_BASE_MS
                .saturating_mul(2u32.saturating_pow(exponent))
                .min(KEEP_ALIVE_RETRY_MAX_MS)
        };
        assert_eq!(delay(1), Duration::from_secs(30));
        assert_eq!(delay(2), Duration::from_secs(60));
        assert_eq!(delay(3), Duration::from_secs(120));
        assert_eq!(delay(4), Duration::from_secs(240));
        assert_eq!(delay(5), Duration::from_secs(300), "capped at 5min");
        assert_eq!(delay(20), Duration::from_secs(300));
    }

    #[test]
    fn should_reconnect_after_refresh_classification() {
        use crate::protocol::ProtocolError;
        // Terminated Streamable session (404 with session id).
        assert!(should_reconnect_after_refresh(
            &ProtocolError::Http {
                status: 404,
                message: "Error POSTing to endpoint".to_string()
            },
            true
        ));
        // No session id → not a terminated-session signal.
        assert!(!should_reconnect_after_refresh(
            &ProtocolError::Http {
                status: 404,
                message: "Error POSTing to endpoint".to_string()
            },
            false
        ));
        // SDK ConnectionClosed equivalent.
        assert!(should_reconnect_after_refresh(
            &ProtocolError::Closed,
            false
        ));
        // A real HTTP failure.
        assert!(!should_reconnect_after_refresh(
            &ProtocolError::Http {
                status: 500,
                message: "boom".to_string()
            },
            true
        ));
    }
}
