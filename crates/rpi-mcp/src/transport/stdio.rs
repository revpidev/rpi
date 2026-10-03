//! stdio transport: child-process management with LF-delimited JSON-RPC
//! framing (port of `packages/mcp/src/transports/stdio.ts` @ a13d35a74).
//!
//! Shutdown follows the spec: close stdin and let the server exit, then
//! SIGTERM the process group, then SIGKILL after `close_timeout_ms`. The
//! process group covers wrappers like `npx`/`uvx`, so children do not
//! outlive the direct child (G4 red line).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex as AsyncMutex;

use super::{DEFAULT_MAX_MESSAGE_BYTES, McpTransport, McpTransportError, TransportEvents};
use crate::protocol::parse_json_rpc_message;

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
}

/// stdio transport (`StdioTransport`, stdio.ts:67).
pub struct StdioTransport {
    options: StdioTransportOptions,
    events: Arc<TransportEvents>,
    state: Arc<StdioState>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stdout_buffer: Arc<Mutex<Vec<u8>>>,
    started: AtomicBool,
    closed: AtomicBool,
    pid: Mutex<Option<u32>>,
}

impl StdioTransport {
    pub fn new(options: StdioTransportOptions) -> Self {
        Self {
            options,
            events: Arc::new(TransportEvents::default()),
            state: Arc::new(StdioState {
                child: AsyncMutex::new(None),
                stdin: AsyncMutex::new(None),
            }),
            stderr: Arc::new(Mutex::new(Vec::new())),
            stdout_buffer: Arc::new(Mutex::new(Vec::new())),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            pid: Mutex::new(None),
        }
    }

    /// `get pid` (stdio.ts:79).
    pub fn pid(&self) -> Option<u32> {
        *lock(&self.pid)
    }

    /// `get stderr` (stdio.ts:83): captured tail of the server's stderr.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&lock(&self.stderr)).into_owned()
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
            // Negative pid: the whole group, so wrappers like `npx` or `uvx`
            // do not leave the server behind.
            // SAFETY: `kill` with a negative pid targets the process group;
            // failure (group gone) is ignored exactly like upstream.
            unsafe {
                libc::kill(-(pid as i32), signal);
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

    async fn try_wait(&self) -> Option<std::process::ExitStatus> {
        let mut guard = self.state.child.lock().await;
        match guard.as_mut() {
            Some(child) => child.try_wait().ok().flatten(),
            None => None,
        }
    }

    fn start_monitor(&self) {
        let state = self.state.clone();
        let events = self.events.clone();
        let stdout_buffer = self.stdout_buffer.clone();
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
            StdioTransport::kill_process_tree(pid, KillSignal::Term);
            if !String::from_utf8_lossy(&lock(&stdout_buffer))
                .trim()
                .is_empty()
            {
                events.emit_error(&McpTransportError::Other(
                    "MCP stdio server closed with an incomplete JSON-RPC message".to_owned(),
                ));
            }
            lock(&stdout_buffer).clear();
            events.emit_close();
        });
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
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *self.state.stdin.lock().await = child.stdin.take();
        *self.state.child.lock().await = Some(child);

        let reader = Arc::new(StdioChildReader {
            events: self.events.clone(),
            stdout_buffer: self.stdout_buffer.clone(),
            stderr: self.stderr.clone(),
            on_stderr: self.options.on_stderr.clone(),
            max_message_bytes: self
                .options
                .max_message_bytes
                .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES),
            max_stderr_bytes: self
                .options
                .max_stderr_bytes
                .unwrap_or(DEFAULT_MAX_STDERR_BYTES),
        });
        if let Some(stdout) = stdout {
            let reader = reader.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stdout);
                let mut buffer = [0u8; 8192];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => reader.handle_stdout(&buffer[..read]),
                    }
                }
            });
        }
        if let Some(stderr) = stderr {
            let reader = reader.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stderr);
                let mut buffer = [0u8; 4096];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => reader.handle_stderr(&buffer[..read]),
                    }
                }
            });
        }
        self.start_monitor();
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
        let close_timeout = Duration::from_millis(
            self.options
                .close_timeout_ms
                .unwrap_or(DEFAULT_CLOSE_TIMEOUT_MS),
        );
        let grace = Duration::from_millis(STDIN_CLOSE_GRACE_MS).min(close_timeout);
        if !wait_for_exit(self, grace).await {
            // SIGTERM, then give the server the close timeout to exit.
            StdioTransport::kill_process_tree(pid, KillSignal::Term);
            if !wait_for_exit(self, close_timeout).await {
                StdioTransport::kill_process_tree(pid, KillSignal::Kill);
                let _ = wait_for_exit(self, Duration::from_millis(500)).await;
            }
        }
        // Reap anything the server left behind even when it exited on its own.
        StdioTransport::kill_process_tree(pid, KillSignal::Term);
        *self.state.child.lock().await = None;
        self.events.emit_close();
        Ok(())
    }

    fn stderr_tail(&self) -> Option<String> {
        Some(self.stderr())
    }

    fn events(&self) -> Arc<TransportEvents> {
        self.events.clone()
    }
}

async fn wait_for_exit(transport: &StdioTransport, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if transport.try_wait().await.is_some() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(EXIT_POLL_INTERVAL).await;
    }
}

/// stdout/stderr reader shared between the two reader tasks.
struct StdioChildReader {
    events: Arc<TransportEvents>,
    stdout_buffer: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    on_stderr: Option<StderrCallback>,
    max_message_bytes: usize,
    max_stderr_bytes: usize,
}

impl StdioChildReader {
    fn handle_stdout(&self, chunk: &[u8]) {
        let mut buffer = lock(&self.stdout_buffer);
        buffer.extend_from_slice(chunk);
        loop {
            let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') else {
                if buffer.len() > self.max_message_bytes {
                    buffer.clear();
                    drop(buffer);
                    self.events.emit_error(&McpTransportError::Other(format!(
                        "MCP stdio message exceeds {} bytes",
                        self.max_message_bytes
                    )));
                    return;
                }
                return;
            };
            let line: Vec<u8> = buffer.drain(..=newline).collect();
            let line = &line[..line.len() - 1];
            if line.len() > self.max_message_bytes {
                drop(buffer);
                self.events.emit_error(&McpTransportError::Other(format!(
                    "MCP stdio message exceeds {} bytes",
                    self.max_message_bytes
                )));
                buffer = lock(&self.stdout_buffer);
                continue;
            }
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let text = String::from_utf8_lossy(line);
            if text.trim().is_empty() {
                continue;
            }
            let parsed = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(parse_json_rpc_message);
            match parsed {
                Some(message) => {
                    drop(buffer);
                    self.events.emit_message(&message);
                    buffer = lock(&self.stdout_buffer);
                }
                None => {
                    drop(buffer);
                    self.events.emit_error(&McpTransportError::Other(format!(
                        "MCP stdio server sent an invalid JSON-RPC message: {}",
                        truncate(&text, 200)
                    )));
                    buffer = lock(&self.stdout_buffer);
                }
            }
        }
    }

    fn handle_stderr(&self, chunk: &[u8]) {
        {
            let mut buffer = lock(&self.stderr);
            buffer.extend_from_slice(chunk);
            if buffer.len() > self.max_stderr_bytes {
                let drop = buffer.len() - self.max_stderr_bytes;
                buffer.drain(..drop);
            }
        }
        if let Some(on_stderr) = &self.on_stderr {
            on_stderr(&String::from_utf8_lossy(chunk));
        }
    }
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_char_boundaries() {
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("ab", 5), "ab");
        assert_eq!(truncate("日本語", 2), "日本");
    }
}
