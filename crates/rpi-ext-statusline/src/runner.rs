//! Script execution (TE12 FR-E): run the configured command with the
//! session JSON on stdin, collect stdout.
//!
//! The command runs through the shell (`sh -c` / `cmd /C`, matching CC —
//! the config value is a command line like `python3 ~/statusline.py`, not a
//! program path). Child-process plumbing follows the subagents precedents:
//! piped stdio with a detached stdin writer (worktree.rs:296-356 — a script
//! that never reads stdin must not deadlock the writer) and a
//! select-based watchdog (foreground.rs:338+) that kills and REAPS on
//! timeout or cancellation (`kill_on_drop` as the belt-and-braces zombie
//! guard).

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Notify;

/// Cancellation handle (CC "a new update cancels the in-flight script").
/// Cloning shares the token; [`CancelToken::cancel`] wakes the single
/// waiter. `Notify::notify_one` stores a permit, so a cancellation issued
/// before the `select!` arms are polled is still observed.
#[derive(Clone)]
pub struct CancelToken(std::sync::Arc<Notify>);

impl CancelToken {
    pub fn new() -> Self {
        Self(std::sync::Arc::new(Notify::new()))
    }

    /// Wake the waiter (single-consumer: the refresh loop's `run` call).
    pub fn cancel(&self) {
        self.0.notify_one();
    }

    async fn notified(&self) {
        self.0.notified().await
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Script failure modes (FR-F: all leave the previous render in place).
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptError {
    /// spawn failure, or cancelled by a newer trigger.
    Spawn(String),
    /// Ran past `timeoutMs` and was killed.
    Timeout { ms: u64 },
    /// Non-zero exit; `stderr` carries the trailing ~200 chars.
    Exit { code: Option<i32>, stderr: String },
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScriptError::Spawn(message) => write!(f, "spawn: {message}"),
            ScriptError::Timeout { ms } => write!(f, "timeout after {ms}ms"),
            ScriptError::Exit { code, stderr } => write!(f, "exit {code:?}: {stderr}"),
        }
    }
}

/// Run `command` once: stdin = `stdin_json` (write-all, then close), env
/// inherited, cwd = the session cwd. Returns the raw stdout (lossy UTF-8).
///
/// On timeout or cancellation the child is killed and reaped before
/// returning, so no process outlives the call.
pub async fn run(
    cancel: CancelToken,
    command: &str,
    stdin_json: &Value,
    cwd: &str,
    timeout_ms: u64,
) -> Result<String, ScriptError> {
    let mut child = shell_command(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ScriptError::Spawn(error.to_string()))?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Detached stdin writer: a script that ignores stdin must not deadlock
    // us on a full pipe (worktree.rs precedent).
    let payload = serde_json::to_vec(stdin_json).unwrap_or_default();
    let writer = tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.shutdown().await;
            // stdin drops here: the script sees EOF.
        }
    });

    // stdout/stderr read to completion in parallel with `wait`, so a
    // script filling both pipes cannot block on our unread buffers.
    let mut stdout_buffer = Vec::new();
    let mut stderr_buffer = Vec::new();
    let reader = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut stderr = stderr;
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_end(&mut stdout_buffer).await;
        }
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_end(&mut stderr_buffer).await;
        }
        (stdout_buffer, stderr_buffer)
    });

    let outcome = tokio::select! {
        status = child.wait() => {
            let (stdout, stderr) = reader.await.unwrap_or_default();
            // The child exited, so the pipes are at EOF; the writer either
            // finished or hits a broken pipe immediately.
            let _ = writer.await;
            match status {
                Ok(status) if status.success() => Ok(stdout),
                Ok(status) => Err(ScriptError::Exit {
                    code: status.code(),
                    stderr: tail(&String::from_utf8_lossy(&stderr), 200),
                }),
                Err(error) => Err(ScriptError::Spawn(error.to_string())),
            }
        }
        _ = cancel.notified() => {
            reader.abort();
            let _ = writer.await;
            reap(child).await;
            Err(ScriptError::Spawn("cancelled".to_owned()))
        }
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            reader.abort();
            let _ = writer.await;
            reap(child).await;
            Err(ScriptError::Timeout { ms: timeout_ms })
        }
    };
    outcome.map(|stdout| String::from_utf8_lossy(&stdout).into_owned())
}

/// `sh -c <command>` on unix, `cmd /C <command>` on Windows (CC semantics:
/// the configured value is a shell command line; `~` expansion happens in
/// the shell).
fn shell_command(command_line: &str) -> Command {
    if cfg!(windows) {
        let mut shell = Command::new("cmd");
        shell.arg("/C").arg(command_line);
        shell
    } else {
        let mut shell = Command::new("sh");
        shell.arg("-c").arg(command_line);
        shell
    }
}

/// Kill and collect the child so no zombie remains (the foreground.rs
/// watchdog discipline, single-step: SIGKILL via start_kill, then reap).
async fn reap(mut child: tokio::process::Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Trailing `max` characters of `text` (whitespace-collapsed, cut on a
/// char boundary).
fn tail(text: &str, max: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= max {
        return collapsed;
    }
    let mut start = collapsed.len() - max;
    while !collapsed.is_char_boundary(start) {
        start -= 1;
    }
    collapsed[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    #[test]
    fn echo_roundtrip_and_cwd() {
        let out = runtime().block_on(run(
            CancelToken::new(),
            "echo hello-statusline",
            &json!({"x": 1}),
            "/tmp",
            5000,
        ));
        assert_eq!(out.expect("echo succeeds").trim(), "hello-statusline");
    }

    #[test]
    fn stdin_json_reaches_the_script() {
        let out = runtime().block_on(run(
            CancelToken::new(),
            "cat",
            &json!({"hook_event_name": "Status", "n": 42}),
            "/tmp",
            5000,
        ));
        assert_eq!(
            out.expect("cat succeeds"),
            r#"{"hook_event_name":"Status","n":42}"#
        );
    }

    #[test]
    fn script_that_never_reads_stdin_still_completes() {
        // Regression shape of the worktree.rs deadlock: payload larger
        // than a pipe buffer, script ignores stdin.
        let big = json!({"blob": "x".repeat(256 * 1024)});
        let out = runtime().block_on(run(
            CancelToken::new(),
            "echo ignored-stdin",
            &big,
            "/tmp",
            10_000,
        ));
        assert_eq!(
            out.expect("echo succeeds").trim(),
            "ignored-stdin",
            "writer must not deadlock on the full pipe"
        );
    }

    #[test]
    fn nonzero_exit_reports_stderr_tail() {
        let error = runtime()
            .block_on(run(
                CancelToken::new(),
                "echo boom >&2; exit 3",
                &json!({}),
                "/tmp",
                5000,
            ))
            .expect_err("exit 3 fails");
        match error {
            ScriptError::Exit { code, stderr } => {
                assert_eq!(code, Some(3));
                assert!(stderr.contains("boom"), "stderr: {stderr}");
            }
            other => panic!("expected Exit, got {other:?}"),
        }
    }

    #[test]
    fn timeout_kills_and_reaps() {
        let start = std::time::Instant::now();
        let error = runtime()
            .block_on(run(CancelToken::new(), "sleep 30", &json!({}), "/tmp", 300))
            .expect_err("sleep times out");
        assert_eq!(error, ScriptError::Timeout { ms: 300 });
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout must not wait for the full sleep"
        );
    }

    #[test]
    fn cancel_kills_inflight_script() {
        let cancel = CancelToken::new();
        let token = cancel.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            token.cancel();
        });
        let start = std::time::Instant::now();
        let error = runtime()
            .block_on(run(cancel, "sleep 30", &json!({}), "/tmp", 60_000))
            .expect_err("cancelled");
        handle.join().expect("cancel thread");
        assert_eq!(error, ScriptError::Spawn("cancelled".to_owned()));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn missing_cwd_is_a_spawn_error() {
        let error = runtime()
            .block_on(run(
                CancelToken::new(),
                "echo hi",
                &json!({}),
                "/definitely/not/a/directory",
                5000,
            ))
            .expect_err("spawn fails");
        assert!(matches!(error, ScriptError::Spawn(_)));
    }

    #[test]
    fn stderr_tail_truncates_on_char_boundary() {
        let text = "a b c ".repeat(100);
        let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let tail = tail(&text, 10);
        assert_eq!(tail.len(), 10, "bounded tail");
        assert_eq!(tail, &collapsed[collapsed.len() - 10..]);
    }
}
