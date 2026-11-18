//! stdio transport: child-process management with LF-delimited JSON-RPC
//! framing (port of `packages/mcp/src/transports/stdio.ts` @ b4afdddf).
//!
//! Shutdown follows the spec: close stdin and let the server exit, then
//! SIGTERM the process group, then SIGKILL after `close_timeout_ms`. The
//! process group covers wrappers like `npx`/`uvx`, so children do not
//! outlive the direct child (G4 red line).

#[cfg(unix)]
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::sync::{Once, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify};

use super::{DEFAULT_MAX_MESSAGE_BYTES, McpTransport, McpTransportError, TransportEvents};

/// `DEFAULT_MAX_STDERR_BYTES` (stdio.ts:7).
const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024;
/// `DEFAULT_CLOSE_TIMEOUT_MS` (stdio.ts:8).
const DEFAULT_CLOSE_TIMEOUT_MS: u64 = 2_000;
/// `STDIN_CLOSE_GRACE_MS` (stdio.ts:10): how long a server gets to exit on
/// its own after stdin closes before SIGTERM.
const STDIN_CLOSE_GRACE_MS: u64 = 500;
/// How often the monitor polls for the child's exit. `tokio::process::Child`
/// can be waited on exactly once, and spawn/close race for it, so both paths
/// observe the exit through `try_wait`.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[cfg(unix)]
fn live_process_groups() -> &'static Mutex<BTreeSet<i32>> {
    LIVE_PROCESS_GROUPS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

#[cfg(unix)]
extern "C" fn terminate_live_process_groups() {
    let Some(groups) = LIVE_PROCESS_GROUPS.get() else {
        return;
    };
    for pid in lock(groups).iter().copied() {
        // SAFETY: each tracked id is a process group created by this module.
        // This is an ordinary exit callback, not a signal handler.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
    }
}

#[cfg(unix)]
static LIVE_PROCESS_GROUPS: OnceLock<Mutex<BTreeSet<i32>>> = OnceLock::new();

#[cfg(unix)]
fn track_process_group(pid: u32) {
    static EXIT_HOOK: Once = Once::new();
    EXIT_HOOK.call_once(|| {
        // SAFETY: the callback remains valid for the life of the process and
        // only signals groups still owned by a live transport.
        unsafe {
            libc::atexit(terminate_live_process_groups);
        }
    });
    lock(live_process_groups()).insert(pid as i32);
}

#[cfg(unix)]
fn untrack_process_group(pid: u32) {
    lock(live_process_groups()).remove(&(pid as i32));
}

/// stderr line callback (`onStderr`, stdio.ts:61).
pub type StderrCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// `stderr` option (stdio.ts:59).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StderrMode {
    #[default]
    Pipe,
    Inherit,
}

/// `StdioTransportOptions` (stdio.ts:52).
#[derive(Clone)]
pub struct StdioTransportOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    /// Inherit the process environment under `env` (default true).
    pub inherit_env: bool,
    pub stderr: StderrMode,
    pub on_stderr: Option<StderrCallback>,
    pub max_message_bytes: Option<usize>,
    pub max_stderr_bytes: Option<usize>,
    pub close_timeout_ms: Option<u64>,
}

impl StdioTransportOptions {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            inherit_env: true,
            stderr: StderrMode::Pipe,
            on_stderr: None,
            max_message_bytes: None,
            max_stderr_bytes: None,
            close_timeout_ms: None,
        }
    }
}

/// Shared process handles; the monitor task and the transport both need
/// access without borrowing the transport.
struct StdioState {
    child: AsyncMutex<Option<Child>>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    disconnected: AtomicBool,
    disconnect: Notify,
    #[cfg(not(unix))]
    exited: AtomicBool,
}

/// stdio transport (`StdioTransport`, stdio.ts:67).
pub struct StdioTransport {
    options: StdioTransportOptions,
    events: Arc<TransportEvents>,
    state: Arc<StdioState>,
    lines: Arc<Mutex<super::lines::JsonRpcLineReader>>,
    started: AtomicBool,
    closed: AtomicBool,
    pid: Arc<Mutex<Option<u32>>>,
}

impl StdioTransport {
    pub fn new(options: StdioTransportOptions) -> Self {
        let events = Arc::new(TransportEvents::default());
        let lines = super::lines::JsonRpcLineReader::new(
            events.clone(),
            options.on_stderr.clone(),
            options
                .max_message_bytes
                .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES),
            options.max_stderr_bytes.unwrap_or(DEFAULT_MAX_STDERR_BYTES),
        );
        Self {
            options,
            events,
            state: Arc::new(StdioState {
                child: AsyncMutex::new(None),
                stdin: AsyncMutex::new(None),
                disconnected: AtomicBool::new(false),
                disconnect: Notify::new(),
                #[cfg(not(unix))]
                exited: AtomicBool::new(false),
            }),
            lines: Arc::new(Mutex::new(lines)),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            pid: Arc::new(Mutex::new(None)),
        }
    }

    /// `get pid` (stdio.ts:79).
    pub fn pid(&self) -> Option<u32> {
        *lock(&self.pid)
    }

    /// `get stderr` (stdio.ts:83): captured tail of the server's stderr.
    pub fn stderr(&self) -> String {
        lock(&self.lines).stderr()
    }

    fn spawn(&self) -> Result<Child, McpTransportError> {
        let mut command = Command::new(&self.options.command);
        command
            .args(&self.options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        command.stderr(match self.options.stderr {
            StderrMode::Pipe => Stdio::piped(),
            StderrMode::Inherit => Stdio::inherit(),
        });
        if let Some(cwd) = &self.options.cwd {
            command.current_dir(cwd);
        }
        if self.options.inherit_env {
            // `{...process.env, ...options.env}` (stdio.ts:97).
            let mut env: HashMap<String, String> = std::env::vars_os()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect();
            for (key, value) in &self.options.env {
                env.insert(key.clone(), value.clone());
            }
            command.env_clear().envs(env);
        } else {
            command.env_clear().envs(self.options.env.clone());
        }
        #[cfg(unix)]
        {
            // Own process group so the whole tree can be signaled
            // (`detached: USE_PROCESS_GROUPS`, stdio.ts:20/104).
            command.process_group(0);
        }
        command.spawn().map_err(|error| {
            McpTransportError::Io(format!("failed to spawn {}: {error}", self.options.command))
        })
    }

    /// `killProcessTree` (stdio.ts:24): the process group where possible,
    /// the direct child otherwise.
    fn kill_process_tree(pid: Option<u32>, signal: KillSignal) {
        let Some(pid) = pid else {
            return;
        };
        #[cfg(unix)]
        {
            let signal = match signal {
                KillSignal::Term => libc::SIGTERM,
                KillSignal::Kill => libc::SIGKILL,
            };
            // Negative pid targets the process group. If the group is gone
            // or was never created, upstream falls back to the direct child.
            // SAFETY: both forms target only the child or its owned group.
            unsafe {
                if libc::kill(-(pid as i32), signal) != 0 {
                    libc::kill(pid as i32, signal);
                }
            }
        }
        #[cfg(windows)]
        {
            // Windows has no graceful signals; `taskkill /T /F` reaps the
            // tree (stdio.ts:31). Best effort.
            let _ = std::process::Command::new("taskkill")
                .args(["/pid", &pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = signal;
            let _ = pid;
        }
    }

    fn start_monitor(&self, readers: [Option<tokio::task::JoinHandle<()>>; 2]) {
        let state = self.state.clone();
        let events = self.events.clone();
        let lines = self.lines.clone();
        let pid_cell = self.pid.clone();
        let pid = *lock(&self.pid);
        tokio::spawn(async move {
            loop {
                let status = {
                    let mut guard = state.child.lock().await;
                    match guard.as_mut() {
                        Some(child) => child.try_wait(),
                        None => return,
                    }
                };
                match status {
                    Ok(Some(_)) => break,
                    Ok(None) => tokio::time::sleep(EXIT_POLL_INTERVAL).await,
                    Err(_) => break,
                }
            }
            #[cfg(not(unix))]
            state.exited.store(true, Ordering::SeqCst);
            // A server can exit while its children keep the inherited pipes
            // open. Keep the group live until those pipes actually close.
            for reader in readers.into_iter().flatten() {
                let _ = reader.await;
            }
            StdioTransport::kill_process_tree(pid, KillSignal::Term);
            #[cfg(unix)]
            if let Some(pid) = pid {
                untrack_process_group(pid);
            }
            *lock(&pid_cell) = None;
            *state.child.lock().await = None;
            state.disconnected.store(true, Ordering::SeqCst);
            state.disconnect.notify_waiters();
            lock(&lines).end();
            events.emit_close();
        });
    }
}

#[cfg(unix)]
impl Drop for StdioTransport {
    fn drop(&mut self) {
        let pid = lock(&self.pid).take();
        if let Some(pid) = pid {
            // No caller remains to run the graceful close ladder.
            Self::kill_process_tree(Some(pid), KillSignal::Kill);
            untrack_process_group(pid);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum KillSignal {
    Term,
    Kill,
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(McpTransportError::Other(
                "MCP stdio transport already started".to_owned(),
            ));
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        let mut child = self.spawn()?;
        *lock(&self.pid) = child.id();
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            track_process_group(pid);
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *self.state.stdin.lock().await = child.stdin.take();
        *self.state.child.lock().await = Some(child);

        let lines = self.lines.clone();
        let stdout = stdout.map(|stdout| {
            let lines = lines.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stdout);
                let mut buffer = [0u8; 8192];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => lock(&lines).read_stdout(&buffer[..read]),
                    }
                }
            })
        });
        let stderr = stderr.map(|stderr| {
            tokio::spawn(async move {
                let mut stream = BufReader::new(stderr);
                let mut buffer = [0u8; 4096];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => lock(&lines).read_stderr(&buffer[..read]),
                    }
                }
            })
        });
        self.start_monitor([stdout, stderr]);
        Ok(())
    }

    async fn send(&self, message: serde_json::Value) -> Result<(), McpTransportError> {
        if !self.started.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        let payload = format!("{message}\n");
        let mut guard = self.state.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(McpTransportError::ConnectionClosed);
        };
        stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|error| McpTransportError::Io(error.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|error| McpTransportError::Io(error.to_string()))
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Close stdin and let the server exit.
        drop(self.state.stdin.lock().await.take());
        let pid = *lock(&self.pid);
        if pid.is_none() {
            self.events.emit_close();
            return Ok(());
        }
        #[cfg(not(unix))]
        if self.state.exited.load(Ordering::SeqCst) {
            return Ok(());
        }
        let close_timeout = Duration::from_millis(
            self.options
                .close_timeout_ms
                .unwrap_or(DEFAULT_CLOSE_TIMEOUT_MS),
        );
        let grace = Duration::from_millis(STDIN_CLOSE_GRACE_MS).min(close_timeout);
        if !wait_for_disconnect(&self.state, grace).await {
            StdioTransport::kill_process_tree(pid, KillSignal::Term);
            if !wait_for_disconnect(&self.state, close_timeout).await {
                StdioTransport::kill_process_tree(pid, KillSignal::Kill);
                wait_until_disconnected(&self.state).await;
            }
        }
        *self.state.child.lock().await = None;
        Ok(())
    }

    fn stderr_tail(&self) -> Option<String> {
        Some(self.stderr())
    }

    fn events(&self) -> Arc<TransportEvents> {
        self.events.clone()
    }
}

async fn wait_until_disconnected(state: &StdioState) {
    loop {
        let notified = state.disconnect.notified();
        tokio::pin!(notified);
        // Register before re-checking: notify_waiters stores no permit.
        notified.as_mut().enable();
        if state.disconnected.load(Ordering::SeqCst) {
            break;
        }
        notified.await;
    }
}

async fn wait_for_disconnect(state: &StdioState, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, wait_until_disconnected(state))
        .await
        .is_ok()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn process_group_failure_falls_back_to_the_direct_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn direct child");
        StdioTransport::kill_process_tree(Some(child.id()), KillSignal::Term);
        let mut exited = false;
        for _ in 0..100 {
            if child.try_wait().expect("child status").is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
            panic!("direct child was not signaled after the process-group miss");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn disconnect_waiters_observe_the_final_notification() {
        let state = Arc::new(StdioState {
            child: AsyncMutex::new(None),
            stdin: AsyncMutex::new(None),
            disconnected: AtomicBool::new(false),
            disconnect: Notify::new(),
            #[cfg(not(unix))]
            exited: AtomicBool::new(false),
        });
        let waiters: Vec<_> = (0..32)
            .map(|_| {
                let state = state.clone();
                tokio::spawn(async move { wait_until_disconnected(&state).await })
            })
            .collect();
        tokio::task::yield_now().await;
        state.disconnected.store(true, Ordering::SeqCst);
        state.disconnect.notify_waiters();
        let results =
            tokio::time::timeout(Duration::from_secs(1), futures::future::join_all(waiters))
                .await
                .expect("all waiters observe disconnect");
        assert!(results.into_iter().all(|result| result.is_ok()));
    }
}
