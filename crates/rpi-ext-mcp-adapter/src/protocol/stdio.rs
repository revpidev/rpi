//! stdio transport: `tokio::process` child spawn, LF-delimited JSON-RPC
//! framing, stderr tail capture for connection-failure diagnostics
//! (FR-P0-07, design §3.1).
//!
//! Port of the stdio branch of `server-manager.ts` + the SDK 2.0
//! `StdioClientTransport` framing @ pi-mcp-adapter v2.24.0 (3d953f90):
//! - spawn `{command} {args}` with env = process env + interpolated
//!   overrides (`resolveEnv`, server-manager.ts:1106-1118) and cwd =
//!   `resolveConfigPath(definition.cwd) ?? <session cwd>`;
//! - messages are single-line JSON on stdout, `\n`-terminated on stdin;
//! - stderr is piped by default and only the trailing 8 KiB / last 3 lines
//!   are retained (`MAX_CAPTURED_STDERR_BYTES/LINES`); `debug: true`
//!   inherits stderr instead;
//! - close drops stdin, waits briefly, then SIGKILLs the child (the G4
//!   no-leftover-process red line).
//!
//! [VARIANT] The npx/npm parent-process resolution optimization
//! (`npx-resolver.ts`) is not ported (requirements FR-P0-07); the command is
//! spawned as configured.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{McpTransport, ProtocolError};
use crate::metadata::ServerEntry;

/// `MAX_CAPTURED_STDERR_BYTES` (server-manager.ts:60).
const MAX_CAPTURED_STDERR_BYTES: usize = 8 * 1024;
/// `MAX_CAPTURED_STDERR_LINES` (server-manager.ts:61).
const MAX_CAPTURED_STDERR_LINES: usize = 3;
/// Grace period between closing stdin and SIGKILL on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Shared stderr tail (`stderrTail` in server-manager.ts).
#[derive(Default)]
pub struct StderrTail {
    bytes: Vec<u8>,
}

impl StderrTail {
    /// `appendStderrTail` (server-manager.ts:110-118): keep the last
    /// `MAX_CAPTURED_STDERR_BYTES` bytes.
    pub fn append(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let start = chunk.len().saturating_sub(MAX_CAPTURED_STDERR_BYTES);
        self.bytes.extend_from_slice(&chunk[start..]);
        if self.bytes.len() > MAX_CAPTURED_STDERR_BYTES {
            let drop = self.bytes.len() - MAX_CAPTURED_STDERR_BYTES;
            self.bytes.drain(..drop);
        }
    }

    /// The failure-diagnostic suffix (server-manager.ts:509-517): trimmed,
    /// last 3 non-empty lines joined with " — ".
    pub fn diagnostic(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.bytes).trim().to_string();
        let lines: Vec<&str> = text
            .split("\n")
            .map(|line| line.trim_matches('\r').trim())
            .filter(|line| !line.is_empty())
            .collect();
        if lines.is_empty() {
            return None;
        }
        let tail: Vec<&str> = lines
            .iter()
            .rev()
            .take(MAX_CAPTURED_STDERR_LINES)
            .rev()
            .copied()
            .collect();
        Some(tail.join(" — "))
    }
}

/// `resolveEnv` (server-manager.ts:1106-1118): process env plus per-server
/// overrides with `!command` secrets resolved at spawn time. Resolution
/// failures abort the spawn (upstream `resolveCommandSecret` throws).
/// `literalEnv` (Agent Plugins, P2) skips interpolation but is accepted for
/// forward compatibility.
pub fn resolve_env(definition: &ServerEntry) -> Result<Vec<(String, String)>, ProtocolError> {
    // Node's `process.env` decodes non-UTF-8 values lossily (U+FFFD);
    // `vars_os` + `to_string_lossy` matches that instead of panicking on
    // `vars()` when the host environment carries binary values.
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let overrides = crate::utils::resolve_command_secrets_record(definition.get("env"), &|key| {
        format!("MCP server stdio env {key:?}")
    })
    .map_err(|e| ProtocolError::Transport(e.to_string()))?
    .unwrap_or_default();
    for (key, value) in overrides {
        let value = match value.as_str() {
            Some(v) => v.to_string(),
            None => continue,
        };
        if let Some(existing) = env.iter_mut().find(|(k, _)| *k == key) {
            existing.1 = value;
        } else {
            env.push((key, value));
        }
    }
    Ok(env)
}

/// A spawned stdio child plus its pipes (the transport's owned state).
pub struct StdioChild {
    child: tokio::sync::Mutex<Child>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pub stderr_tail: Arc<Mutex<StderrTail>>,
    /// stdout/stderr reader tasks; `shutdown` awaits them so the captured
    /// stderr tail is complete before the connection error is reported.
    readers: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl StdioChild {
    /// Spawn the child and start the stdout reader / stderr capture tasks.
    /// Returns the transport-ready child plus the incoming-message receiver.
    pub fn spawn(
        definition: &ServerEntry,
        default_cwd: Option<&str>,
        incoming: mpsc::UnboundedSender<Value>,
    ) -> Result<Arc<Self>, ProtocolError> {
        let command = definition
            .get_str("command")
            .filter(|c| !c.is_empty())
            .ok_or_else(|| ProtocolError::Transport("missing command".to_string()))?;
        let args: Vec<String> = match definition.get("args") {
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(Value::as_str)
                .map(crate::utils::interpolate_env_vars)
                .collect(),
            _ => Vec::new(),
        };
        let cwd = crate::utils::resolve_config_path(definition.get("cwd"))
            .ok()
            .flatten()
            .or_else(|| default_cwd.map(str::to_string));
        let debug = definition.get("debug") == Some(&Value::Bool(true));

        let mut cmd = Command::new(command);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if debug {
                Stdio::inherit()
            } else {
                Stdio::piped()
            })
            // Own process group so a shutdown can reap the whole tree
            // (coding-standards §11.3).
            .process_group(0)
            .kill_on_drop(true);
        cmd.env_clear().envs(resolve_env(definition)?);
        if let Some(cwd) = &cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ProtocolError::Transport(format!("failed to spawn {command}: {e}")))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let mut readers = Vec::new();
        // stdout reader: LF-delimited JSON (SDK ReadBuffer semantics).
        if let Some(stdout) = stdout {
            readers.push(tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut buf: Vec<u8> = Vec::with_capacity(4096);
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    // SDK ReadBuffer: `Buffer.toString("utf8")` decodes
                    // lossily (invalid bytes -> U+FFFD) and a line that then
                    // fails JSON.parse is skipped (`SyntaxError` continue),
                    // so invalid UTF-8 must never sever the stream.
                    let line_bytes = buf.strip_suffix(b"\n").unwrap_or(&buf);
                    let line_bytes = line_bytes.strip_suffix(b"\r").unwrap_or(line_bytes);
                    let line = match std::str::from_utf8(line_bytes) {
                        Ok(line) => line.to_string(),
                        Err(error) => {
                            warn!(%error, "MCP stdio: non-UTF-8 line on stdout; decoding lossily");
                            String::from_utf8_lossy(line_bytes).into_owned()
                        }
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(&line) {
                        Ok(message) => {
                            if incoming.send(message).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            debug!(%error, "MCP stdio: dropping non-JSON line");
                        }
                    }
                }
            }));
        }

        let stderr_tail: Arc<Mutex<StderrTail>> = Arc::new(Mutex::new(StderrTail::default()));
        if let Some(stderr) = stderr {
            let tail = stderr_tail.clone();
            readers.push(tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut buf = vec![0u8; 4096];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => tail
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .append(&buf[..n]),
                    }
                }
            }));
        }

        Ok(Arc::new(Self {
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(stdin),
            stderr_tail,
            readers: tokio::sync::Mutex::new(readers),
        }))
    }

    /// Close stdin, wait for exit, then SIGKILL the process group after the
    /// grace period. Never leaves a running child behind (G4 red line).
    pub async fn shutdown(&self) {
        let pid = {
            let mut stdin = self.stdin.lock().await;
            drop(stdin.take());
            let child = self.child.lock().await;
            child.id()
        };
        let exited = {
            let mut child = self.child.lock().await;
            tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await
        };
        if exited.is_err() {
            warn!(
                ?pid,
                "MCP stdio child did not exit after stdin close; killing"
            );
            // Kill the whole process group first (grandchildren included),
            // then the child itself as a fallback for non-unix targets.
            #[cfg(unix)]
            if let Some(pid) = pid {
                // Safety: kill(2) with a process-group id; no memory access.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let mut child = self.child.lock().await;
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        // Join the readers (they end at EOF) so the stderr tail is complete.
        // A setsid-escaped grandchild holding the pipes can pin a reader
        // forever; bound the join like the grace ladder above and abandon
        // the task on timeout so shutdown always returns.
        let mut readers = self.readers.lock().await;
        for reader in readers.drain(..) {
            if tokio::time::timeout(SHUTDOWN_GRACE, reader).await.is_err() {
                warn!("MCP stdio: reader task did not finish after kill; abandoning join");
            }
        }
    }
}

/// The stdio transport handle held by the client.
pub struct StdioTransport {
    child: Arc<StdioChild>,
}

impl StdioTransport {
    pub fn child(&self) -> &Arc<StdioChild> {
        &self.child
    }
}

#[async_trait::async_trait]
impl McpTransport for StdioTransport {
    async fn send(&self, message: Value) -> Result<(), ProtocolError> {
        let mut bytes = serde_json::to_vec(&message)
            .map_err(|e| ProtocolError::Transport(format!("serialize: {e}")))?;
        bytes.push(b'\n');
        let mut guard = self.child.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(ProtocolError::Closed);
        };
        stdin
            .write_all(&bytes)
            .await
            .map_err(|e| ProtocolError::Transport(format!("stdin write: {e}")))
    }

    async fn close(&self) -> Result<(), ProtocolError> {
        self.child.shutdown().await;
        Ok(())
    }
}

/// Spawn a stdio transport for `definition` (the `command` branch of
/// `createConnection`, server-manager.ts:359-390).
pub fn connect_stdio(
    definition: &ServerEntry,
    default_cwd: Option<&str>,
) -> Result<(StdioTransport, mpsc::UnboundedReceiver<Value>), ProtocolError> {
    let (tx, rx) = mpsc::unbounded_channel();
    let child = StdioChild::spawn(definition, default_cwd, tx)?;
    Ok((StdioTransport { child }, rx))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn stderr_tail_keeps_last_bytes_and_lines() {
        let mut tail = StderrTail::default();
        tail.append(b"line one\n");
        tail.append(b"line two\nline three\nline four\n");
        assert_eq!(
            tail.diagnostic().as_deref(),
            Some("line two — line three — line four")
        );
    }

    #[test]
    fn stderr_tail_caps_at_8kib() {
        let mut tail = StderrTail::default();
        tail.append(&vec![b'x'; 16 * 1024]);
        assert_eq!(tail.bytes.len(), MAX_CAPTURED_STDERR_BYTES);
    }

    #[test]
    fn stderr_tail_empty_has_no_diagnostic() {
        assert_eq!(StderrTail::default().diagnostic(), None);
    }

    #[test]
    fn resolve_env_overlays_interpolated_overrides() {
        std::env::set_var("RPI_MCP_STDIO_TEST", "resolved");
        let definition = ServerEntry(
            json!({ "env": { "RPI_MCP_STDIO_TEST": "$env:RPI_MCP_STDIO_TEST", "EXTRA": "x" } })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let env = resolve_env(&definition).expect("env resolves");
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("RPI_MCP_STDIO_TEST").as_deref(), Some("resolved"));
        assert_eq!(get("EXTRA").as_deref(), Some("x"));
        std::env::remove_var("RPI_MCP_STDIO_TEST");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_env_survives_non_utf8_values() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let key = "RPI_MCP_STDIO_BIN_TEST";
        std::env::set_var(key, OsString::from_vec(vec![0xff, 0xfe, b'A']));
        let definition = ServerEntry(serde_json::Map::new());
        let env = resolve_env(&definition).expect("env resolves");
        std::env::remove_var(key);
        let value = env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        // Lossy decode, matching Node's `process.env` (U+FFFD per bad byte).
        assert_eq!(value.as_deref(), Some("\u{fffd}\u{fffd}A"));
    }

    #[tokio::test]
    async fn stdout_reader_survives_invalid_utf8_line() {
        // First line is a lone invalid byte; the connection must stay up
        // and deliver the JSON line after it (SDK ReadBuffer lossy decode
        // + skip semantics).
        let definition = ServerEntry(
            json!({ "command": "sh", "args": ["-c", "printf '\\xff\\n{\"x\":1}\\n'"] })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let child = StdioChild::spawn(&definition, None, tx).expect("spawn");
        let message = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for message")
            .expect("channel closed without a message");
        assert_eq!(message["x"], json!(1));
        child.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_abandons_stuck_reader() {
        // A reader that never reaches EOF must not hang shutdown: the join
        // times out and shutdown returns.
        let child = Command::new("true").spawn().expect("spawn true");
        let stuck = StdioChild {
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(None),
            stderr_tail: Arc::new(Mutex::new(StderrTail::default())),
            readers: tokio::sync::Mutex::new(vec![tokio::spawn(async {
                std::future::pending::<()>().await
            })]),
        };
        let started = std::time::Instant::now();
        stuck.shutdown().await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= SHUTDOWN_GRACE,
            "join must have gone through its timeout, waited {elapsed:?}"
        );
        assert!(
            elapsed < SHUTDOWN_GRACE + Duration::from_secs(10),
            "shutdown must not block on a stuck reader, took {elapsed:?}"
        );
    }
}
