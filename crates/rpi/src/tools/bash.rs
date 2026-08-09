//! Port of `packages/coding-agent/src/core/tools/bash.ts` @ pi 0.82.1 (2efa728).
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use rpi_ai::types::{TextContent, ToolResultContent};

use crate::tools::output_accumulator::{
    OutputAccumulator, OutputAccumulatorOptions, OutputSnapshot,
};
use crate::tools::truncate::{format_size, TruncatedBy, DEFAULT_MAX_BYTES};
use crate::tools::{SessionEnv, ToolContext};

pub const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
pub const MAX_TIMEOUT_SECONDS: f64 = MAX_TIMEOUT_MS as f64 / 1000.0;
const BASH_UPDATE_THROTTLE_MS: u64 = 100;
const EXIT_STDIO_GRACE_MS: u64 = 100;

#[derive(Debug, thiserror::Error)]
pub enum BashExecError {
    #[error("aborted")]
    Aborted,
    #[error("timeout:{0}")]
    Timeout(f64),
    #[error("{0}")]
    Message(String),
}

pub struct BashExecOptions {
    pub signal: CancellationToken,
    pub timeout: Option<f64>,
    pub env: Option<HashMap<String, String>>,
}

#[async_trait]
pub trait BashOperations: Send + Sync {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        options: BashExecOptions,
        on_data: &(dyn Fn(Vec<u8>) + Send + Sync),
    ) -> Result<Option<i32>, BashExecError>;
}

pub struct BashSpawnContext {
    pub command: String,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}
pub type BashSpawnHook = Arc<dyn Fn(BashSpawnContext) -> BashSpawnContext + Send + Sync>;

pub struct BashToolOptions {
    pub operations: Option<Arc<dyn BashOperations>>,
    pub command_prefix: Option<String>,
    pub shell_path: Option<String>,
    pub expose_session_environment: bool,
    pub spawn_hook: Option<BashSpawnHook>,
}

impl Default for BashToolOptions {
    fn default() -> Self {
        Self {
            operations: None,
            command_prefix: None,
            shell_path: None,
            expose_session_environment: true,
            spawn_hook: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandTransport {
    Argv,
    Stdin,
}

#[derive(Debug, Clone)]
struct ShellConfig {
    shell: String,
    args: Vec<String>,
    command_transport: CommandTransport,
}

static WSL_BASH_REGEX: OnceLock<Option<Regex>> = OnceLock::new();

fn is_legacy_wsl_bash_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_lowercase();
    let re = WSL_BASH_REGEX
        .get_or_init(|| Regex::new(r"^[a-z]:\\windows\\(?:system32|sysnative)\\bash\.exe$").ok());
    match re {
        Some(r) => r.is_match(&normalized),
        None => false,
    }
}

fn get_bash_shell_config(shell: &str) -> ShellConfig {
    if is_legacy_wsl_bash_path(shell) {
        ShellConfig {
            shell: shell.to_string(),
            args: vec!["-s".into()],
            command_transport: CommandTransport::Stdin,
        }
    } else {
        ShellConfig {
            shell: shell.to_string(),
            args: vec!["-c".into()],
            command_transport: CommandTransport::Argv,
        }
    }
}

fn find_bash_on_path() -> Option<String> {
    let output = std::process::Command::new("which")
        .arg("bash")
        .output()
        .ok()?;
    if output.status.success() {
        let s = String::from_utf8_lossy(&output.stdout);
        let f = s.lines().next()?;
        if !f.is_empty() {
            return Some(f.to_string());
        }
    }
    None
}

fn get_shell_config(custom: Option<&str>) -> Result<ShellConfig, BashExecError> {
    if let Some(c) = custom {
        if Path::new(c).exists() {
            return Ok(get_bash_shell_config(c));
        }
        return Err(BashExecError::Message(format!(
            "Custom shell path not found: {c}"
        )));
    }
    if Path::new("/bin/bash").exists() {
        return Ok(get_bash_shell_config("/bin/bash"));
    }
    if let Some(b) = find_bash_on_path() {
        return Ok(get_bash_shell_config(&b));
    }
    Ok(ShellConfig {
        shell: "sh".into(),
        args: vec!["-c".into()],
        command_transport: CommandTransport::Argv,
    })
}

fn get_shell_env() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        let bin = PathBuf::from(&home).join(".rpi").join("bin");
        let bins = bin.to_string_lossy().into_owned();
        let key = env
            .keys()
            .find(|k| k.eq_ignore_ascii_case("path"))
            .cloned()
            .unwrap_or_else(|| "PATH".into());
        let cur = env.get(&key).cloned().unwrap_or_default();
        let has = cur.split(':').filter(|s| !s.is_empty()).any(|e| e == bins);
        if !has {
            let np = if cur.is_empty() {
                bins
            } else {
                format!("{bins}:{cur}")
            };
            env.insert(key, np);
        }
    }
    env
}

#[cfg(unix)]
fn kill_process_tree(pid: u32) {
    let pgid = pid as i32;
    if unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 {
        let _ = unsafe { libc::kill(pgid, libc::SIGKILL) };
    }
}
#[cfg(not(unix))]
fn kill_process_tree(_pid: u32) {}

fn resolve_timeout_ms(timeout: Option<f64>) -> Result<Option<u64>, BashExecError> {
    let t = match timeout {
        None => return Ok(None),
        Some(t) => t,
    };
    if !t.is_finite() || t <= 0.0 {
        return Err(BashExecError::Message(
            "Invalid timeout: must be a finite number of seconds".into(),
        ));
    }
    let ms = t * 1000.0;
    if ms > MAX_TIMEOUT_MS as f64 {
        return Err(BashExecError::Message(format!(
            "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
        )));
    }
    Ok(Some(ms as u64))
}

enum IoMsg {
    Data(Vec<u8>),
    StdoutEof,
    StderrEof,
}

pub struct LocalBashOperations {
    shell_path: Option<String>,
}

#[async_trait]
impl BashOperations for LocalBashOperations {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        options: BashExecOptions,
        on_data: &(dyn Fn(Vec<u8>) + Send + Sync),
    ) -> Result<Option<i32>, BashExecError> {
        let timeout_ms = resolve_timeout_ms(options.timeout)?;
        if options.signal.is_cancelled() {
            return Err(BashExecError::Aborted);
        }
        let sc = get_shell_config(self.shell_path.as_deref())?;
        if !cwd.exists() {
            return Err(BashExecError::Message(format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                cwd.display()
            )));
        }
        let env = options.env.unwrap_or_else(get_shell_env);
        let from_stdin = sc.command_transport == CommandTransport::Stdin;
        let mut cmd = tokio::process::Command::new(&sc.shell);
        cmd.args(&sc.args);
        if !from_stdin {
            cmd.arg(command);
        }
        cmd.current_dir(cwd).env_clear();
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if from_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        // Detached process group on Unix (bash.ts:99 — `detached: true`).
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| BashExecError::Message(format!("Failed to spawn process: {e}")))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, mut rx) = mpsc::channel::<IoMsg>(64);
        if let Some(mut s) = stdout {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) => {
                            let _ = tx.send(IoMsg::StdoutEof).await;
                            break;
                        }
                        Ok(n) => {
                            if tx.send(IoMsg::Data(buf[..n].to_vec())).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(IoMsg::StdoutEof).await;
                            break;
                        }
                    }
                }
            });
        }
        if let Some(mut s) = stderr {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) => {
                            let _ = tx.send(IoMsg::StderrEof).await;
                            break;
                        }
                        Ok(n) => {
                            if tx.send(IoMsg::Data(buf[..n].to_vec())).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(IoMsg::StderrEof).await;
                            break;
                        }
                    }
                }
            });
        }
        drop(tx);
        if from_stdin {
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(command.as_bytes()).await;
                let _ = stdin.shutdown().await;
            }
        }
        let mut timed_out = false;
        let mut aborted = false;
        let mut exited = false;
        let mut exit_code: Option<i32> = None;
        let mut stdout_ended = false;
        let mut stderr_ended = false;
        let mut pending_data: Option<Vec<u8>> = None;
        tokio::pin! { let timeout_fut = match timeout_ms { Some(ms) => futures::future::Either::Left(tokio::time::sleep(Duration::from_millis(ms))), None => futures::future::Either::Right(std::future::pending()), }; }
        loop {
            tokio::select! {
                biased;
                _ = options.signal.cancelled(), if !aborted && !timed_out => { aborted = true; if let Some(p) = pid { kill_process_tree(p); } }
                _ = &mut timeout_fut, if !timed_out && !aborted => { timed_out = true; if let Some(p) = pid { kill_process_tree(p); } }
                result = child.wait(), if !exited => { exited = true; exit_code = result.ok().and_then(|s| s.code()); }
                msg = rx.recv() => { match msg { Some(IoMsg::Data(b)) => { pending_data = Some(b); } Some(IoMsg::StdoutEof) => { stdout_ended = true; } Some(IoMsg::StderrEof) => { stderr_ended = true; } None => { stdout_ended = true; stderr_ended = true; } } }
            }
            if let Some(b) = pending_data.take() {
                on_data(b);
            }
            if exited {
                break;
            }
        }
        if !stdout_ended || !stderr_ended {
            let idle = tokio::time::sleep(Duration::from_millis(EXIT_STDIO_GRACE_MS));
            tokio::pin!(idle);
            loop {
                let mut d: Option<Vec<u8>> = None;
                tokio::select! {
                    biased;
                    msg = rx.recv() => { match msg { Some(IoMsg::Data(b)) => { d = Some(b); idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(EXIT_STDIO_GRACE_MS)); } Some(IoMsg::StdoutEof) => { stdout_ended = true; if stderr_ended { break; } } Some(IoMsg::StderrEof) => { stderr_ended = true; if stdout_ended { break; } } None => { break; } } }
                    _ = &mut idle => { break; }
                }
                if let Some(b) = d.take() {
                    on_data(b);
                }
            }
        }
        if aborted {
            return Err(BashExecError::Aborted);
        }
        if timed_out {
            return Err(BashExecError::Timeout(options.timeout.unwrap_or(0.0)));
        }
        Ok(exit_code)
    }
}

pub fn create_local_bash_operations(shell_path: Option<String>) -> Arc<dyn BashOperations> {
    Arc::new(LocalBashOperations { shell_path })
}

struct BashTool {
    cwd: PathBuf,
    session_env: Option<std::sync::Arc<std::sync::RwLock<SessionEnv>>>,
    ops: Arc<dyn BashOperations>,
    command_prefix: Option<String>,
    expose_session_environment: bool,
    spawn_hook: Option<BashSpawnHook>,
    parameters_value: Value,
    description_str: String,
}

fn make_details(snapshot: &OutputSnapshot) -> Value {
    let mut map = serde_json::Map::new();
    if snapshot.truncation.truncated {
        if let Ok(v) = serde_json::to_value(&snapshot.truncation) {
            map.insert("truncation".into(), v);
        }
    }
    if let Some(ref p) = snapshot.full_output_path {
        map.insert(
            "fullOutputPath".into(),
            Value::String(p.to_string_lossy().into_owned()),
        );
    }
    Value::Object(map)
}

fn format_output(
    snapshot: &OutputSnapshot,
    last_line_bytes: usize,
    empty_text: &str,
) -> (String, Value) {
    let tr = &snapshot.truncation;
    let base = if snapshot.text.is_empty() {
        empty_text.to_string()
    } else {
        snapshot.text.clone()
    };
    if !tr.truncated {
        return (base, Value::Null);
    }
    let details = make_details(snapshot);
    let start = tr.total_lines - tr.output_lines + 1;
    let end = tr.total_lines;
    let full = snapshot
        .full_output_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let suffix = if tr.last_line_partial {
        format!(
            "\n\n[Showing last {} of line {} (line is {}). Full output: {}]",
            format_size(tr.output_bytes),
            end,
            format_size(last_line_bytes),
            full
        )
    } else if tr.truncated_by == Some(TruncatedBy::Lines) {
        format!(
            "\n\n[Showing lines {}-{} of {}. Full output: {}]",
            start, end, tr.total_lines, full
        )
    } else {
        format!(
            "\n\n[Showing lines {}-{} of {} ({} limit). Full output: {}]",
            start,
            end,
            tr.total_lines,
            format_size(DEFAULT_MAX_BYTES),
            full
        )
    };
    (format!("{base}{suffix}"), details)
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

impl BashTool {
    fn resolve_spawn_context(&self, command: &str) -> BashSpawnContext {
        let cwd = self.cwd.clone();
        let mut env = get_shell_env();
        for k in &[
            "RPI_SESSION_ID",
            "RPI_SESSION_FILE",
            "RPI_PROVIDER",
            "RPI_MODEL",
            "RPI_REASONING_LEVEL",
        ] {
            env.remove(*k);
        }
        if self.expose_session_environment {
            if let Some(ref cell) = self.session_env {
                let s = cell.read().unwrap_or_else(|e| e.into_inner());
                env.insert("RPI_SESSION_ID".into(), s.session_id.clone());
                if let Some(ref f) = s.session_file {
                    env.insert("RPI_SESSION_FILE".into(), f.to_string_lossy().into_owned());
                }
                if let Some(ref p) = s.provider {
                    env.insert("RPI_PROVIDER".into(), p.clone());
                }
                if let Some(ref m) = s.model {
                    env.insert("RPI_MODEL".into(), m.clone());
                }
                if let Some(ref l) = s.reasoning_level {
                    env.insert("RPI_REASONING_LEVEL".into(), l.clone());
                }
            }
        }
        let ctx = BashSpawnContext {
            command: command.to_string(),
            cwd,
            env,
        };
        match &self.spawn_hook {
            Some(h) => h(ctx),
            None => ctx,
        }
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn label(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        &self.description_str
    }
    fn parameters(&self) -> &Value {
        &self.parameters_value
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, AgentError> {
        let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let timeout = params.get("timeout").and_then(|v| v.as_f64());
        let resolved = match &self.command_prefix {
            Some(p) => format!("{p}\n{command}"),
            None => command.to_string(),
        };
        let spawn_ctx = self.resolve_spawn_context(&resolved);
        let accumulator: Arc<Mutex<OutputAccumulator>> = Arc::new(Mutex::new(
            OutputAccumulator::new(OutputAccumulatorOptions {
                temp_file_prefix: "pi-bash".into(),
                ..Default::default()
            }),
        ));
        if let Some(ref cb) = on_update {
            cb(AgentToolResult::default());
        }
        let (update_tx, update_handle) = match on_update {
            Some(cb) => {
                let (tx, mut rx) = mpsc::unbounded_channel::<()>();
                let acc = Arc::clone(&accumulator);
                let h = tokio::spawn(async move {
                    let throttle = Duration::from_millis(BASH_UPDATE_THROTTLE_MS);
                    let mut last = std::time::Instant::now() - throttle - throttle;
                    while rx.recv().await.is_some() {
                        // Drain any already-queued notifications so we only emit
                        // one update per throttle window (coalescing, bash.ts:369-382).
                        while rx.try_recv().is_ok() {}
                        let el = last.elapsed();
                        if el < throttle {
                            tokio::time::sleep(throttle - el).await;
                        }
                        // Drain again after sleeping — new data may have arrived.
                        while rx.try_recv().is_ok() {}
                        let snap = {
                            let mut a = acc.lock().unwrap_or_else(|e| e.into_inner());
                            a.snapshot(true)
                        };
                        let det = make_details(&snap);
                        cb(AgentToolResult {
                            content: vec![ToolResultContent::Text(TextContent {
                                text: snap.text,
                                text_signature: None,
                            })],
                            details: det,
                            ..Default::default()
                        });
                        last = std::time::Instant::now();
                    }
                });
                (Some(tx), Some(h))
            }
            None => (None, None),
        };
        let acc2 = Arc::clone(&accumulator);
        let tx2 = update_tx.clone();
        let on_data = move |data: Vec<u8>| {
            let mut a = acc2.lock().unwrap_or_else(|e| e.into_inner());
            a.append(&data);
            if let Some(ref tx) = tx2 {
                let _ = tx.send(());
            }
        };
        let exec_result = self
            .ops
            .exec(
                &spawn_ctx.command,
                &spawn_ctx.cwd,
                BashExecOptions {
                    signal,
                    timeout,
                    env: Some(spawn_ctx.env),
                },
                &on_data,
            )
            .await;
        // Drop on_data (which holds tx2, a clone of update_tx) so the update
        // task's rx.recv() returns None and the task terminates.
        drop(on_data);
        drop(update_tx);
        if let Some(h) = update_handle {
            let _ = h.await;
        }
        let (snapshot, last_lb, details) = {
            let mut a = accumulator.lock().unwrap_or_else(|e| e.into_inner());
            a.finish();
            let lb = a.last_line_bytes();
            let snap = a.snapshot(true);
            a.close_temp_file();
            let det = if snap.truncation.truncated {
                make_details(&snap)
            } else {
                Value::Null
            };
            (snap, lb, det)
        };
        match exec_result {
            Ok(exit_code) => {
                let (text, _) = format_output(&snapshot, last_lb, "(no output)");
                if let Some(code) = exit_code {
                    if code != 0 {
                        return Err(AgentError::Message(append_status(
                            &text,
                            &format!("Command exited with code {code}"),
                        )));
                    }
                }
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent {
                        text,
                        text_signature: None,
                    })],
                    details,
                    ..Default::default()
                })
            }
            Err(e) => {
                let (text, _) = format_output(&snapshot, last_lb, "");
                match e {
                    BashExecError::Aborted => {
                        Err(AgentError::Message(append_status(&text, "Command aborted")))
                    }
                    BashExecError::Timeout(secs) => Err(AgentError::Message(append_status(
                        &text,
                        &format!("Command timed out after {secs} seconds"),
                    ))),
                    BashExecError::Message(m) => Err(AgentError::Message(m)),
                }
            }
        }
    }
}

pub fn create_bash_tool(ctx: &ToolContext, options: BashToolOptions) -> Arc<dyn AgentTool> {
    let ops = options
        .operations
        .clone()
        .unwrap_or_else(|| create_local_bash_operations(options.shell_path.clone()));
    let desc = "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.";
    let params = json!({ "type": "object", "properties": { "command": { "type": "string", "description": "Bash command to execute" }, "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" } }, "required": ["command"] });
    Arc::new(BashTool {
        cwd: ctx.cwd.clone(),
        session_env: ctx.session_env.clone(),
        ops,
        command_prefix: options.command_prefix.clone(),
        expose_session_environment: options.expose_session_environment,
        spawn_hook: options.spawn_hook.clone(),
        parameters_value: params,
        description_str: desc.to_string(),
    })
}
