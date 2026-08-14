//! Foreground blocking child run: spawn, stream parse, timeout ladder, exit
//! synthesis, artifacts (FR-P0-01/06/09).
//!
//! Port of pi-subagents `src/runs/foreground/execution.ts` `runSync` @
//! v0.48.0 (56f97234) (P0 subset), driving the rpi child process with the
//! argv/env from [`crate::launch::args`].
//!
//! Signal ladders (execution.ts:571-623, 1021-1079):
//! - timeout: SIGINT → 1s → SIGTERM → 4s → SIGKILL;
//! - terminal drain: 1s grace → SIGTERM → 3s → SIGKILL;
//! - shutdown sweep (session_shutdown): SIGTERM → 3s → SIGKILL.
//!
//! Intentional differences: the post-exit stdio guard (idle 2s / hard 8s) is
//! approximated by draining pipes until EOF after exit with the same 8s hard
//! cap (TE-D18); rpi's extension ABI has no toolExecute abort channel, so
//! user-level aborts surface through `kill_on_drop` and the shutdown sweep
//! instead (registered as an ABI candidate gap).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::artifacts::{self, ArtifactPaths};
use crate::launch::args::{self, BuildArgsInput, TaskDelivery};
use crate::launch::binary;
use crate::runner::budget;
use crate::runner::events::{
    self, BoundedByteTail, BoundedLineReader, ChildLifecycleAction, ChildRunState,
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_OUTPUT_LINES, MAX_CHILD_PENDING_LINE_BYTES,
    MAX_CHILD_STDERR_BYTES,
};

pub const DEFAULT_FOREGROUND_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const FINAL_STOP_GRACE_MS: u64 = 1000;
const TIMEOUT_SIGINT_TO_TERM_MS: u64 = 1000;
const TIMEOUT_TERM_TO_KILL_MS: u64 = 4000;
const DRAIN_TERM_TO_KILL_MS: u64 = 3000;
const SHUTDOWN_TERM_TO_KILL_MS: u64 = 3000;
const POST_EXIT_DRAIN_HARD_MS: u64 = 8000;

/// Live children registry for the shutdown sweep. `kill_on_drop(true)` covers
/// future-drop and process-exit; this covers `session_shutdown` events.
type LiveChildEntry = (u32, Arc<tokio::sync::Mutex<Child>>);
static LIVE_CHILDREN: Mutex<BTreeMap<u64, LiveChildEntry>> = Mutex::new(BTreeMap::new());
static NEXT_CHILD_ID: AtomicU64 = AtomicU64::new(1);

fn register_child(child: Child) -> (u64, u32, Arc<tokio::sync::Mutex<Child>>) {
    let id = NEXT_CHILD_ID.fetch_add(1, Ordering::Relaxed);
    let pid = child.id().unwrap_or(0);
    let shared = Arc::new(tokio::sync::Mutex::new(child));
    LIVE_CHILDREN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, (pid, shared.clone()));
    (id, pid, shared)
}

fn unregister_child(id: u64) {
    LIVE_CHILDREN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
}

/// Kill every live child (SIGTERM → 3s → SIGKILL). Called from the
/// `session_shutdown` event handler; runs on the plugin runtime.
pub async fn kill_all_children_for_shutdown() {
    let children: Vec<LiveChildEntry> = {
        LIVE_CHILDREN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    };
    for (pid, child) in children {
        signal_pid(pid, Signal::Term);
        tokio::time::sleep(Duration::from_millis(SHUTDOWN_TERM_TO_KILL_MS)).await;
        signal_pid(pid, Signal::Kill);
        let _ = child.lock().await.wait().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Int,
    Term,
    Kill,
}

/// Signal the child by pid. The child mutex stays with the run (held across
/// `wait()`), so ladders must never contend for it — signals are pid-direct,
/// matching the upstream `proc.kill(...)` calls.
fn signal_pid(pid: u32, signal: Signal) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        let signum = match signal {
            Signal::Int => libc::SIGINT,
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // Safety: kill(2) with a checked pid.
        unsafe {
            libc::kill(pid as i32, signum);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
    }
}

#[derive(Debug, Clone, Default)]
pub struct ForegroundRunInput {
    pub agent_name: String,
    pub agent_system_prompt: String,
    pub agent_system_prompt_mode: &'static str,
    pub agent_tools: Option<Vec<String>>,
    pub agent_extensions: Option<Vec<String>>,
    pub agent_subagent_only_extensions: Option<Vec<String>>,
    pub agent_inherit_project_context: bool,
    pub agent_inherit_skills: bool,
    pub task: String,
    pub task_delivery: Option<TaskDelivery>,
    pub cwd: PathBuf,
    pub session_dir: Option<PathBuf>,
    pub session_file: Option<PathBuf>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub run_id: String,
    pub timeout_ms: Option<u64>,
    /// Effective cap the child's own nesting checks enforce.
    pub child_max_subagent_depth: u64,
    pub artifacts_dir: Option<PathBuf>,
    pub include_jsonl: bool,
    pub include_transcript: bool,
    pub parent_session_id: Option<String>,
    pub self_extension: Option<String>,
    pub fanout_authorized: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ForegroundRunResult {
    pub exit_code: i32,
    pub error: Option<String>,
    pub final_output: String,
    pub usage: Value,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub timed_out: bool,
    pub process_signal: Option<String>,
    pub tool_count: u64,
    pub duration_ms: u64,
    pub artifact_paths: Option<ArtifactPaths>,
    pub session_file: Option<PathBuf>,
    /// Raw assistant/toolResult messages from the child; P0 consumers are
    /// tests (details compact them away upstream).
    #[allow(dead_code)]
    pub messages: Vec<Value>,
    pub truncation: Option<Value>,
    pub attempted_models: Vec<String>,
}

const PROMPT_REDACTED: &str = "[prompt redacted]";

/// Run one child to completion (blocking the caller's async context — the
/// plugin runs it on its private runtime while the host dispatch thread
/// waits).
pub async fn run_foreground(input: &ForegroundRunInput) -> ForegroundRunResult {
    let start = std::time::Instant::now();
    let artifact_paths = input
        .artifacts_dir
        .as_ref()
        .map(|dir| artifacts::get_artifact_paths(dir, &input.run_id, &input.agent_name, Some(0)));
    let mut attempted_models = Vec::new();
    if let Some(model) = &input.model {
        attempted_models.push(model.clone());
    }

    let launch = args::build_rpi_args(&BuildArgsInput {
        base_args: vec!["--mode".into(), "json".into(), "-p".into()],
        task: input.task.clone(),
        task_delivery: input.task_delivery,
        session_enabled: true,
        session_dir: input.session_dir.clone(),
        session_file: input.session_file.clone(),
        model: input.model.clone(),
        thinking: input.thinking.clone(),
        system_prompt: Some(input.agent_system_prompt.clone()),
        system_prompt_mode: input.agent_system_prompt_mode,
        inherit_project_context: input.agent_inherit_project_context,
        inherit_skills: input.agent_inherit_skills,
        require_read_tool: false,
        tools: input.agent_tools.clone(),
        extensions: input.agent_extensions.clone(),
        subagent_only_extensions: input.agent_subagent_only_extensions.clone(),
        mcp_direct_tools: Vec::new(),
        prompt_file_stem: Some(input.agent_name.clone()),
        run_id: Some(input.run_id.clone()),
        child_agent_name: Some(input.agent_name.clone()),
        child_index: Some(0),
        parent_session_id: input.parent_session_id.clone(),
        fanout_authorized: input.fanout_authorized,
        self_extension: input.self_extension.clone(),
    });

    let launch = match launch {
        Ok(launch) => launch,
        Err(error) => {
            return failed_result(
                input,
                artifact_paths,
                attempted_models,
                start,
                error.to_string(),
            )
        }
    };

    // Depth env rides on top of the shared env (execution.ts:476).
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    for (key, value) in &launch.env {
        match value {
            Some(value) => {
                env.insert(key.clone(), value.clone());
            }
            None => {
                env.remove(key);
            }
        }
    }
    for (key, value) in budget::depth_env_for_child(input.child_max_subagent_depth) {
        env.insert(key, value);
    }

    let spawn_command = binary::resolve_spawn_command(&launch.args);
    let mut command = Command::new(&spawn_command.program);
    command
        .args(&spawn_command.args)
        .current_dir(&input.cwd)
        .env_clear()
        .envs(&env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // New process group so our signals don't fan out to the host's group.
        command.process_group(0);
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            args::cleanup_temp_dir(&launch.temp_dir);
            return failed_result(
                input,
                artifact_paths,
                attempted_models,
                start,
                format!(
                    "Failed to spawn subagent process '{}': {}",
                    spawn_command.program, error
                ),
            );
        }
    };

    if let Some(paths) = &artifact_paths {
        let _ = artifacts::ensure_artifacts_dir(paths.input_path.parent().unwrap_or(&input.cwd));
        let _ = artifacts::write_artifact(
            &paths.input_path,
            &format!(
                "# Task for {}\n\n{}; live Prompt Audit only.\n",
                input.agent_name, PROMPT_REDACTED
            ),
        );
    }

    let (child_id, child_pid, child_handle) = register_child(child);
    // Take the pipes under a short lock; the run must NOT hold the child lock
    // while streams are open (the wait below re-acquires it).
    let (stdout, stderr) = {
        let mut guard = child_handle.lock().await;
        (guard.stdout.take(), guard.stderr.take())
    };

    let state = Arc::new(Mutex::new(ChildRunState::default()));
    let raw_tail = Arc::new(Mutex::new(BoundedByteTail::new(MAX_CHILD_STDERR_BYTES)));
    let jsonl_path = artifact_paths
        .as_ref()
        .filter(|_| input.include_jsonl)
        .map(|p| p.jsonl_path.clone());
    let transcript_path = artifact_paths
        .as_ref()
        .filter(|_| input.include_transcript)
        .map(|p| p.transcript_path.clone());
    let stderr_transcript_path = transcript_path.clone();

    // Shared run flags.
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let terminal_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stream_error = Arc::new(Mutex::new(None::<String>));

    // Timeout ladder: SIGINT → 1s → SIGTERM → 4s → SIGKILL (1021-1044).
    let timeout_ms = input.timeout_ms;
    let timed_out_flag = timed_out.clone();
    let timeout_task = tokio::spawn(async move {
        let Some(timeout_ms) = timeout_ms else {
            return;
        };
        tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
        timed_out_flag.store(true, Ordering::SeqCst);
        signal_pid(child_pid, Signal::Int);
        tokio::time::sleep(Duration::from_millis(TIMEOUT_SIGINT_TO_TERM_MS)).await;
        signal_pid(child_pid, Signal::Term);
        tokio::time::sleep(Duration::from_millis(TIMEOUT_TERM_TO_KILL_MS)).await;
        signal_pid(child_pid, Signal::Kill);
    });

    // Terminal-drain ladder: after the terminal event, 1s grace then
    // SIGTERM → 3s → SIGKILL (571-623). Upstream records the
    // "did not exit within 1000ms" error only when the drain fired WITHOUT a
    // clean terminal state (watchdog/budget triggers — P1 surfaces); the P0
    // trigger set only starts this ladder on a clean terminal, so no error is
    // recorded here.
    let terminal_flag = terminal_seen.clone();
    let drain_task = tokio::spawn(async move {
        loop {
            if terminal_flag.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(FINAL_STOP_GRACE_MS)).await;
        signal_pid(child_pid, Signal::Term);
        tokio::time::sleep(Duration::from_millis(DRAIN_TERM_TO_KILL_MS)).await;
        signal_pid(child_pid, Signal::Kill);
    });

    // stdout reader: bounded line reader + event accumulation + artifacts.
    let stdout_state = state.clone();
    let stdout_raw = raw_tail.clone();
    let stdout_terminal = terminal_seen.clone();
    let stdout_protocol_error = stream_error.clone();
    let stdout_task = tokio::spawn(async move {
        let Some(mut stdout) = stdout else {
            return;
        };
        let mut buffer = vec![0u8; 64 * 1024];
        let mut reader_state = ChildRunState::default();
        let mut terminal = false;

        // Protocol-limit path (execution.ts:1047-1062): record the error and
        // run the SIGTERM → 3s → SIGKILL ladder.
        let on_limit = {
            let error_slot = stdout_protocol_error.clone();
            move |limit: &events::ProtocolOutputLimit| {
                *error_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(limit.message());
                tokio::spawn(async move {
                    signal_pid(child_pid, Signal::Term);
                    tokio::time::sleep(Duration::from_millis(DRAIN_TERM_TO_KILL_MS)).await;
                    signal_pid(child_pid, Signal::Kill);
                });
            }
        };

        let mut line_reader = BoundedLineReader::new(
            "stdout",
            MAX_CHILD_PENDING_LINE_BYTES,
            |line: &str| {
                if let Some(jsonl) = &jsonl_path {
                    artifacts::append_jsonl(jsonl, line);
                }
                if let Some(transcript) = &transcript_path {
                    artifacts::append_jsonl(transcript, &format!("stdout: {line}"));
                }
                let trimmed = line.trim();
                let parsed: Option<Value> = serde_json::from_str(trimmed).ok();
                let is_event = parsed
                    .as_ref()
                    .and_then(|value| value.get("type"))
                    .and_then(Value::as_str)
                    .map(|t| t != "session")
                    .unwrap_or(false);
                if !is_event {
                    stdout_raw
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(line.as_bytes());
                    return;
                }
                let outcome = reader_state.process_line(trimmed);
                match outcome.lifecycle {
                    ChildLifecycleAction::StartDrain => terminal = true,
                    ChildLifecycleAction::CancelDrain => terminal = false,
                    ChildLifecycleAction::None => {}
                }
                stdout_terminal.store(terminal, Ordering::SeqCst);
            },
            on_limit,
        );
        loop {
            match stdout.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => line_reader.push(&buffer[..read]),
                Err(error) => {
                    *stdout_protocol_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("Failed to read subagent stdout: {error}"));
                    break;
                }
            }
        }
        line_reader.end();
        let exceeded = line_reader.exceeded();
        drop(line_reader);
        *stdout_state.lock().unwrap_or_else(|e| e.into_inner()) = reader_state;
        if exceeded {
            // The on_limit callback already recorded the protocol error and
            // started the kill ladder; this keeps the flag observable.
            tracing::debug!("child stdout line limit exceeded");
        }
    });

    // stderr reader: 128 KiB line-capped transcript + ring tail
    // (child-protocol.ts:7, 370-392).
    let stderr_raw = raw_tail.clone();
    let stderr_task = tokio::spawn(async move {
        let Some(mut stderr) = stderr else {
            return;
        };
        let mut buffer = vec![0u8; 16 * 1024];
        let mut line_bytes: Vec<u8> = Vec::new();
        let mut transcript_bytes = 0usize;
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    stderr_raw
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(&buffer[..read]);
                    if let Some(transcript) = &stderr_transcript_path {
                        line_bytes.extend_from_slice(&buffer[..read]);
                        while let Some(pos) = line_bytes.iter().position(|b| *b == b'\n') {
                            let line: Vec<u8> = line_bytes.drain(..=pos).collect();
                            if transcript_bytes < MAX_CHILD_STDERR_BYTES {
                                let text =
                                    String::from_utf8_lossy(&line[..line.len().saturating_sub(1)])
                                        .to_string();
                                artifacts::append_jsonl(transcript, &format!("stderr: {text}"));
                                transcript_bytes += line.len();
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the process itself first — piped readers can outlive the child
    // when a grandchild inherits the descriptors. Post-exit stdio guard
    // (execution.ts:1075 idleMs 2s/hardMs 8s) approximated by the hard cap.
    let exit_status = child_handle.lock().await.wait().await;
    let mut stdout_task = Some(stdout_task);
    let mut stderr_task = Some(stderr_task);
    let drained = tokio::time::timeout(Duration::from_millis(POST_EXIT_DRAIN_HARD_MS), async {
        if let Some(task) = stdout_task.take() {
            let _ = task.await;
        }
        if let Some(task) = stderr_task.take() {
            let _ = task.await;
        }
    })
    .await;
    if drained.is_err() {
        // Grandchild holding the pipes: force-close the readers.
        if let Some(task) = stdout_task.take() {
            task.abort();
        }
        if let Some(task) = stderr_task.take() {
            task.abort();
        }
    }
    timeout_task.abort();
    drain_task.abort();
    unregister_child(child_id);
    args::cleanup_temp_dir(&launch.temp_dir);

    let child_state = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let stream_error_text = stream_error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // Tool availability diagnostic the child wrote (execution.ts:1085-1121
    // `toolDiagnosticError` slot; ADR-0017).
    let tool_diagnostic_error =
        crate::diagnostic::read_child_tool_diagnostic_error(launch.tool_diagnostic_path.as_deref());
    let raw_tail_text = raw_tail.lock().unwrap_or_else(|e| e.into_inner()).text();

    let duration_ms = start.elapsed().as_millis() as u64;
    let mut result = synthesize_exit(
        input,
        child_state,
        exit_status,
        timed_out.load(Ordering::SeqCst),
        stream_error_text.as_deref(),
        tool_diagnostic_error.as_deref(),
        raw_tail_text,
        duration_ms,
    );
    if let Some(paths) = &artifact_paths {
        persist_artifacts(input, paths, &result, &input.task);
    }
    result.artifact_paths = artifact_paths;
    result.attempted_models = attempted_models;
    result.session_file = input.session_file.clone();
    result
}

/// Persist artifacts for a finished run (`persistSingleResultMetadata` P0
/// subset + output/input writes).
fn persist_artifacts(
    input: &ForegroundRunInput,
    paths: &ArtifactPaths,
    result: &ForegroundRunResult,
    _task: &str,
) {
    let output_content = artifacts::format_output_artifact_content(
        &result.final_output,
        result.error.as_deref(),
        Some(&paths.transcript_path),
        Some(&paths.metadata_path),
    );
    let _ = artifacts::write_artifact(&paths.output_path, &output_content);
    let metadata = json!({
        "runId": input.run_id,
        "agent": input.agent_name,
        "task": PROMPT_REDACTED,
        "exitCode": result.exit_code,
        "processSignal": result.process_signal,
        "usage": result.usage,
        "model": result.model,
        "attemptedModels": result.attempted_models,
        "durationMs": result.duration_ms,
        "toolCount": result.tool_count,
        "error": result.error,
        "timestamp": artifacts::format_iso8601(artifacts::now_millis()),
    });
    let _ = artifacts::write_metadata(&paths.metadata_path, &metadata);
}

/// Exit-code and error synthesis (execution.ts:1085-1121, 1204-1247).
/// Priority: timeout > stream/protocol error > toolDiagnosticError >
/// assistantError > unexplained signal > raw non-JSON stdout. Error with a
/// zero exit forces exit 1.
#[allow(clippy::too_many_arguments)]
fn synthesize_exit(
    input: &ForegroundRunInput,
    state: ChildRunState,
    exit_status: std::io::Result<std::process::ExitStatus>,
    timed_out: bool,
    stream_error: Option<&str>,
    tool_diagnostic_error: Option<&str>,
    raw_stdout: String,
    duration_ms: u64,
) -> ForegroundRunResult {
    let final_output_raw = events::get_final_output(&state.messages);
    let status = exit_status.ok();
    #[cfg(unix)]
    let signal_of_status = status.and_then(|status| {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    });
    #[cfg(not(unix))]
    let signal_of_status: Option<i32> = None;

    // finalCode = (signal) ? (code ?? 1) : (code ?? 0) — a forced drain after
    // a clean terminal is upstream's exit-0 case, approximated by a 0 code.
    let mut exit_code = match (&status, signal_of_status) {
        (_, Some(_)) => status.and_then(|s| s.code()).unwrap_or(1),
        (Some(status), _) if status.success() => 0,
        (Some(status), _) => status.code().unwrap_or(0),
        (None, _) => 1,
    };
    let process_signal = signal_of_status.map(signal_name);

    let mut error = if timed_out {
        Some(format!(
            "Subagent timed out after {}ms.",
            input.timeout_ms.unwrap_or_default()
        ))
    } else {
        None
    };
    if error.is_none() {
        if let Some(stream_error) = stream_error {
            error = Some(stream_error.to_string());
        }
    }
    if error.is_none() {
        if let Some(tool_diagnostic) = tool_diagnostic_error {
            error = Some(tool_diagnostic.to_string());
        }
    }
    if error.is_none() {
        if let Some(assistant_error) = &state.assistant_error {
            error = Some(assistant_error.clone());
        }
    }
    // Unexplained signal: a signal we did not order (timed_out covered above).
    if error.is_none() && process_signal.is_some() {
        error = Some(format!(
            "Subagent process terminated by signal {}.",
            process_signal.clone().unwrap_or_default()
        ));
    }
    if error.is_none() && exit_code != 0 && !raw_stdout.trim().is_empty() {
        error = Some(raw_stdout.trim().to_string());
    }

    // Timeout partial-output rewrite (1269-1273).
    let mut final_output = final_output_raw;
    if timed_out && !final_output.trim().is_empty() {
        let timeout_note = format!(
            "Subagent timed out after {}ms.",
            input.timeout_ms.unwrap_or_default()
        );
        final_output = format!("{timeout_note}\n\nPartial output before timeout:\n{final_output}");
    }

    // No-output failure (1231-1247): clean exit, no error, empty output.
    if exit_code == 0
        && error.is_none()
        && final_output.trim().is_empty()
        && !state.messages.is_empty()
    {
        exit_code = 1;
        error = Some(
            "Subagent produced no output (possible model cold-start or empty response)."
                .to_string(),
        );
    }
    if error.is_some() && exit_code == 0 {
        exit_code = 1;
    }

    let output_artifact = input.artifacts_dir.as_ref().map(|dir| {
        artifacts::get_artifact_paths(dir, &input.run_id, &input.agent_name, Some(0))
            .output_path
            .to_string_lossy()
            .to_string()
    });
    let truncation = events::truncate_output(
        &final_output,
        DEFAULT_MAX_OUTPUT_BYTES,
        DEFAULT_MAX_OUTPUT_LINES,
        output_artifact.as_deref(),
    );
    let truncation_json = if truncation.truncated {
        Some(json!({
            "truncated": true,
            "originalBytes": truncation.original_bytes,
            "originalLines": truncation.original_lines,
        }))
    } else {
        None
    };

    ForegroundRunResult {
        exit_code,
        error,
        final_output: truncation.text,
        usage: state.usage_json(),
        model: state.model,
        thinking: input.thinking.clone(),
        timed_out,
        process_signal,
        tool_count: state.tool_count,
        duration_ms,
        artifact_paths: None,
        session_file: None,
        messages: state.messages,
        truncation: truncation_json,
        attempted_models: Vec::new(),
    }
}

fn failed_result(
    input: &ForegroundRunInput,
    artifact_paths: Option<ArtifactPaths>,
    attempted_models: Vec<String>,
    start: std::time::Instant,
    message: String,
) -> ForegroundRunResult {
    ForegroundRunResult {
        exit_code: 1,
        error: Some(message),
        final_output: String::new(),
        usage: events::json_usage(0, 0, 0, 0, 0.0),
        model: None,
        thinking: input.thinking.clone(),
        timed_out: false,
        process_signal: None,
        tool_count: 0,
        duration_ms: start.elapsed().as_millis() as u64,
        artifact_paths,
        session_file: None,
        messages: Vec::new(),
        truncation: None,
        attempted_models,
    }
}

fn signal_name(signal: i32) -> String {
    match signal {
        2 => "SIGINT".to_string(),
        9 => "SIGKILL".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("signal {other}"),
    }
}
