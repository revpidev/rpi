//! Lifecycle state machine: lazy/eager/keep-alive/lazy-keep-alive modes,
//! periodic health checks, idle shutdown, graceful shutdown order
//! (FR-P0-09, design §3.6).
//!
//! Port of `lifecycle.ts` (`McpLifecycleManager`) @ pi-mcp-adapter v2.24.0
//! (3d953f90). The 60s failure backoff tracker from `init.ts`
//! (`recordFailure`/`clearFailure`/`getFailureAgeSeconds`) lives here too —
//! the design assigns init.ts's lifecycle responsibilities to this module.
//!
//! P0 scope notes:
//! - `hasPendingAuthForServer` is always false (OAuth pending state is P1);
//!   the skip-reconnect branch is preserved as a hook.
//! - The health-check interval defaults to 30s and is injectable for tests
//!   (upstream `startHealthChecks(signal, intervalMs)` parameter).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::manager::{ConnectionStatus, McpServerManager};
use crate::metadata::ServerEntry;

/// `FAILURE_BACKOFF_MS` (init.ts:39).
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(60);
/// `MAX_FAILURE_MESSAGE_CHARS` (init.ts:40).
const MAX_FAILURE_MESSAGE_CHARS: usize = 8 * 1024;
/// Default health-check period (design §3.6).
pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Default idle timeout (init.ts:212: `settings.idleTimeout ?? 10` minutes).
pub const DEFAULT_IDLE_TIMEOUT_MINUTES: u64 = 10;

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

/// `McpLifecycleManager` (lifecycle.ts:10-151).
pub struct LifecycleManager {
    manager: Arc<McpServerManager>,
    keep_alive: Mutex<HashMap<String, ServerEntry>>,
    all_servers: Mutex<HashMap<String, (ServerEntry, Option<u64>)>>,
    global_idle_timeout: Mutex<Duration>,
    health_interval: Mutex<Duration>,
    cancel: CancellationToken,
    health_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    check_running: AtomicBool,
    stopped: AtomicBool,
    on_reconnect: Mutex<Option<ReconnectCallback>>,
    on_reconnect_failure: Mutex<Option<ReconnectFailureCallback>>,
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
            stopped: AtomicBool::new(false),
            on_reconnect: Mutex::new(None),
            on_reconnect_failure: Mutex::new(None),
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

    pub fn set_idle_shutdown_callback(&self, callback: ReconnectCallback) {
        *self
            .on_idle_shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    /// `markKeepAlive` (lifecycle.ts:38-41).
    pub fn mark_keep_alive(&self, name: &str, definition: &ServerEntry) {
        if definition.is_disabled() {
            return;
        }
        self.keep_alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), definition.clone());
    }

    /// `registerServer` (lifecycle.ts:43-47).
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

    /// `setGlobalIdleTimeout` (lifecycle.ts:49-51), minutes.
    pub fn set_global_idle_timeout_minutes(&self, minutes: u64) {
        *self
            .global_idle_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Duration::from_secs(minutes * 60);
    }

    /// `getIdleTimeout` (lifecycle.ts:123-127).
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

    /// `startHealthChecks` (lifecycle.ts:57-86).
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

    /// `checkConnections` (lifecycle.ts:88-121).
    async fn check_connections(&self) {
        if self.stopped.load(Ordering::SeqCst) || self.cancel.is_cancelled() {
            return;
        }
        let keep_alive: Vec<(String, ServerEntry)> = self
            .keep_alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (name, definition) in &keep_alive {
            if definition.is_disabled() {
                continue;
            }
            let connected = self
                .manager
                .get_connection(name)
                .is_some_and(|c| c.status() == ConnectionStatus::Connected);
            if connected {
                continue;
            }
            // P0: no OAuth pending state (P1); upstream skips reconnect while
            // an authorization is pending.
            match self.manager.connect(name, definition).await {
                Ok(_) => {
                    if self.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    debug!(server = %name, "MCP: reconnected keep-alive server");
                    let callback = self
                        .on_reconnect
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if let Some(callback) = callback {
                        callback(name);
                    }
                }
                Err(error) => {
                    if self.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    let message = error.to_string();
                    error!(server = %name, %message, "MCP: failed to reconnect");
                    let callback = self
                        .on_reconnect_failure
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if let Some(callback) = callback {
                        callback(name, &message);
                    }
                }
            }
        }

        let all: Vec<String> = self
            .all_servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        let keep_alive_names: HashSet<&str> =
            keep_alive.iter().map(|(name, _)| name.as_str()).collect();
        for name in all {
            if keep_alive_names.contains(name.as_str()) {
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

    /// `gracefulShutdown` (lifecycle.ts:129-150): cancel the health task,
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
        self.manager.close_all().await;
    }
}

/// The failure tracker half of init.ts: `recordFailure` / `clearFailure` /
/// `getFailureAgeSeconds` with the 60s self-expiry (init.ts:39-80, 556-567).
pub struct FailureTracker {
    failed_at: Mutex<HashMap<String, u64>>,
    messages: Mutex<HashMap<String, String>>,
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
        }
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
                    let mut failed = this.failed_at.lock().unwrap_or_else(|e| e.into_inner());
                    if failed.get(&name) == Some(&failed_at) {
                        failed.remove(&name);
                        this.messages
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&name);
                    }
                }
            }
        });
    }

    /// `clearFailure` (init.ts:52-59).
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
}
