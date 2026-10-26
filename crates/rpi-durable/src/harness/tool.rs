//! Built-in tool task (port of `packages/durable/src/harness/tool.ts`
//! @ `4a080252f`).
//!
//! A tool task resolves the called tool among its phase agent's tools, validates
//! the arguments, records the intent, executes the tool, and appends the result
//! entry, all in one `call` handler. The `execute` phase is reached only by
//! recovery and applies the replay rule.
//!
//! Rust shape note (FORM, V17-14): `env` is always `None` and the running-output
//! window is always `None`; `output()` appends to an in-memory buffer that only
//! the terminal result entry carries, and `details()` does not commit progress.
//! The full env/shell/output-window pipeline is V17-15.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_ai::models::Models;
use rpi_ai::types::{
    AssistantContent, Message, TextContent, ToolCall, ToolResultContent, ToolResultMessage,
    ToolResultRole,
};
use rpi_ai::utils::validation::validate_tool_arguments;
use serde::{Deserialize, Serialize};

use crate::chord::context::Context;
use crate::chord::json::{JsonObject, JsonValue};
use crate::entries::TOOL_RESULT_ENTRY;
use crate::harness::agent::agent_hooks;
use crate::harness::live::{LIVE_DOC, LiveState, ToolSlot, finish_slot};
use crate::harness::registry::TOOL_TASK_NAME;
use crate::harness::types::{
    Agent, ConversationHandle, DiagnosticSeverity, HookHandlers, NextTaskState, RegistrySnapshot,
    RunningTask, SettledTask, TaskCommitFn, TaskDefinition, TaskRuntime, ToolCommitFn,
    ToolDiagnostic, ToolExecutionApi, ToolExecutionResult, ToolRegistration, ToolReplay,
};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::ids::{ConversationId, EntryId, TaskId};
use crate::session::{DocOwner, EntryDraft, SessionError, SessionResult, Transaction};
use crate::types::{EntryRecord, TaskOutcome, TaskOutcomeError, TaskRecord};

/// Tool task input: the assistant entry and the call ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTaskInput {
    /// The tool-calling answer.
    pub assistant: EntryId,
    /// Provider call ID.
    pub call_id: String,
}

/// Durable tool checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
enum ToolCheckpoint {
    /// Resolution and validation.
    #[serde(rename = "call")]
    Call,
    /// Durable intent: final arguments and replay policy recorded before execution.
    #[serde(rename = "execute")]
    Execute {
        /// Final validated arguments.
        arguments: JsonObject,
        /// Replay policy at intent time.
        replay: String,
    },
}

/// Tool task result: the appended result entry and optional post-tools controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTaskResult {
    /// Appended result entry.
    pub entry_id: EntryId,
    /// Post-tools controls, when the tool returned any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<crate::harness::types::ToolControl>,
}

/// How one tool task ends.
#[derive(Debug, Clone)]
enum Ending {
    Completed,
    Aborted,
    Failed(String),
}

/// Built-in tool task (`pi.tool`).
pub struct ToolTask;

#[async_trait]
impl TaskDefinition for ToolTask {
    fn name(&self) -> &str {
        TOOL_TASK_NAME
    }

    fn version(&self) -> u64 {
        1
    }

    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "call" })
    }

    async fn run_phase(
        &self,
        phase: &str,
        task: RunningTask,
        runtime: &dyn TaskRuntime,
        context: &Context,
    ) -> SessionResult<()> {
        match phase {
            "call" => call_phase(&task, runtime, context).await,
            "execute" => execute_phase(&task, runtime, context).await,
            _ => Ok(()),
        }
    }

    async fn abort(
        &self,
        task: RunningTask,
        runtime: &dyn TaskRuntime,
        context: &Context,
    ) -> SessionResult<()> {
        let input = parse_input(&task)?;
        let call = read_call(runtime, &input, context).await?;
        let slot = read_slot(runtime, context).await?;
        let message = format!("Tool {} was aborted", call.name);
        let result = from_slot(slot.as_ref(), "aborted", &message);
        settle(runtime, &call, Ending::Aborted, result, context, None).await
    }
}

fn parse_input(task: &RunningTask) -> SessionResult<ToolTaskInput> {
    serde_json::from_value(task.record.input.clone())
        .map_err(|error| SessionError::message(error.to_string()))
}

fn parse_checkpoint(checkpoint: Option<&JsonValue>) -> SessionResult<ToolCheckpoint> {
    match checkpoint {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|error| SessionError::message(error.to_string())),
        None => Ok(ToolCheckpoint::Call),
    }
}

async fn call_phase(
    task: &RunningTask,
    runtime: &dyn TaskRuntime,
    context: &Context,
) -> SessionResult<()> {
    let input = parse_input(task)?;
    let call = read_call(runtime, &input, context).await?;
    let agent = runtime.agent(context).await?;
    let tool = agent
        .tools
        .iter()
        .find(|tool| tool.name() == call.name)
        .cloned();
    let Some(tool) = tool else {
        let error = harness_error(
            "tool_unavailable",
            &format!("Tool {} is not available", call.name),
        );
        return settle(runtime, &call, Ending::Completed, error, context, None).await;
    };
    let prepared = prepare(&tool, call.arguments.clone());
    let checked = match prepared {
        Err(error) => Err(error),
        Ok(args) => validate(&tool, &call, args),
    };
    let mut args = match checked {
        Err(error) => {
            let result = invalid(&error);
            return settle(runtime, &call, Ending::Completed, result, context, None).await;
        }
        Ok(args) => args,
    };
    let mut block: Option<String> = None;
    for handlers in agent_hooks(&agent, TOOL_TASK_NAME) {
        if block.is_some() {
            break;
        }
        if let HookHandlers::Tool(hooks) = handlers {
            let api = crate::harness::types::RuntimeHookApi { runtime };
            let mut call_with_args = call.clone();
            call_with_args.arguments = args.clone();
            if let Some(decision) = hooks.before_tool(&call_with_args, &api, context).await {
                if let Some(reason) = decision.block {
                    block = Some(reason);
                } else if let Some(replacement) = decision.arguments {
                    args = replacement;
                }
            }
        }
    }
    if let Some(reason) = block {
        let result = harness_error("blocked", &format!("Tool call blocked: {reason}"));
        return settle(runtime, &call, Ending::Completed, result, context, None).await;
    }
    let validated = match validate(&tool, &call, args) {
        Ok(args) => args,
        Err(error) => {
            let result = invalid(&error);
            return settle(runtime, &call, Ending::Completed, result, context, None).await;
        }
    };
    let final_args = validated;
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let replay = match tool.replay {
        ToolReplay::Safe => "safe",
        ToolReplay::Unsafe => "unsafe",
    }
    .to_string();
    let intent_args = final_args.clone();
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let intent_args = intent_args.clone();
        let replay = replay.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            if let Some(slots) = state.tools.as_mut()
                && let Some(slot) = slots.iter_mut().find(|slot| slot.task_id == Some(task_id))
            {
                slot.status = crate::harness::live::ToolSlotStatus::Running;
            }
            crate::harness::types::write_state(&live, &state)?;
            Ok(Some(NextTaskState::Running {
                checkpoint: serde_json::to_value(ToolCheckpoint::Execute {
                    arguments: intent_args,
                    replay,
                })
                .unwrap_or(JsonValue::Null),
            }))
        })
    });
    runtime.commit(change, context).await?;
    run(runtime, &call, &tool, final_args, context).await
}

async fn execute_phase(
    task: &RunningTask,
    runtime: &dyn TaskRuntime,
    context: &Context,
) -> SessionResult<()> {
    let checkpoint = parse_checkpoint(task.checkpoint())?;
    let (args, replay) = match checkpoint {
        ToolCheckpoint::Execute { arguments, replay } => (arguments, replay),
        ToolCheckpoint::Call => return Ok(()),
    };
    let input = parse_input(task)?;
    let call = read_call(runtime, &input, context).await?;
    let agent = runtime.agent(context).await?;
    let tool = agent
        .tools
        .iter()
        .find(|tool| tool.name() == call.name)
        .cloned();
    if replay == "safe"
        && let Some(tool) = &tool
        && tool.replay == ToolReplay::Safe
    {
        // The rerun reports from scratch; clear what the interrupted attempt published.
        clear_slot(runtime, context).await?;
        return run(runtime, &call, tool, args, context).await;
    }
    let message = format!(
        "Tool {} was interrupted and may have partially run",
        call.name
    );
    let slot = read_slot(runtime, context).await?;
    let result = from_slot(slot.as_ref(), "interrupted", &message);
    settle(
        runtime,
        &call,
        Ending::Failed(message),
        result,
        context,
        None,
    )
    .await
}

/// Resolved output bounds of one tool call.
#[derive(Debug, Clone, Copy)]
struct OutputLimitsSpec {
    max_bytes: u64,
    max_lines: u64,
    retain: crate::harness::types::OutputRetain,
}

fn resolve_output_limits(limits: Option<crate::harness::types::OutputLimits>) -> OutputLimitsSpec {
    OutputLimitsSpec {
        max_bytes: limits
            .and_then(|limits| limits.max_bytes)
            .unwrap_or(crate::truncate::DEFAULT_MAX_BYTES),
        max_lines: limits
            .and_then(|limits| limits.max_lines)
            .unwrap_or(crate::truncate::DEFAULT_MAX_LINES),
        retain: limits
            .and_then(|limits| limits.retain)
            .unwrap_or(crate::harness::types::OutputRetain::Head),
    }
}

/// What a running tool reported through its api: output, the last details, and
/// diagnostics.
struct Reported {
    output: crate::harness::output::OutputBuffer,
    limits: OutputLimitsSpec,
    diagnostics: Vec<ToolDiagnostic>,
    details: Option<JsonValue>,
}

/// What one progress commit has already written.
struct WrittenReport {
    text: String,
    details: Option<JsonValue>,
    diagnostics: usize,
}

/// Execute with the resolved implementation, run `afterTool`, then settle.
async fn run(
    runtime: &dyn TaskRuntime,
    call: &ToolCall,
    tool: &ToolRegistration,
    args: JsonObject,
    context: &Context,
) -> SessionResult<()> {
    let limits = resolve_output_limits(tool.output_limits);
    let reported = Arc::new(Mutex::new(Reported {
        output: crate::harness::output::OutputBuffer::new(crate::harness::output::OutputLimits {
            max_bytes: limits.max_bytes,
            max_lines: limits.max_lines,
            retain: limits.retain,
        }),
        limits,
        diagnostics: Vec::new(),
        details: None,
    }));
    let progress = publish_progress(runtime, Arc::clone(&reported), context);
    // Built for this call, so a rerun after recovery gets the conversation's
    // environment at that time.
    let env_error: Option<String>;
    let env: Option<Arc<dyn crate::env::types::ExecutionEnv>>;
    match runtime.env(context).await {
        Ok(value) => {
            env = value;
            env_error = None;
        }
        Err(error) => {
            if runtime.signal().is_cancelled() {
                return Err(error);
            }
            env = None;
            env_error = Some(error.to_string());
        }
    }
    let api = ToolApi::new(
        runtime,
        call.id.clone(),
        Arc::clone(&reported),
        env,
        progress.clone(),
    );

    let started = std::time::Instant::now();
    let mut ending = Ending::Completed;
    let result = if let Some(message) = env_error {
        ending = Ending::Failed(format!("Tool {} threw", call.name));
        error_result(&message)
    } else {
        match tool.execute.execute(args, &api, context).await {
            Ok(result) => result,
            Err(error) => {
                if runtime.signal().is_cancelled() {
                    api.mark_ended();
                    let pending = progress.stop().await;
                    reject_waiters(pending, error.to_string());
                    return Err(SessionError::Aborted);
                }
                ending = Ending::Failed(format!("Tool {} threw", call.name));
                error_result(&error.to_string())
            }
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    api.mark_ended();
    if let Ok(mut reported) = reported.lock() {
        reported.output.end();
    }
    // Details still waiting for a progress commit settle with the terminal
    // commit, the final flush.
    let pending = progress.stop().await;
    let settled = final_result(runtime, call, result, &reported, context).await;
    let result = match settled {
        Ok(result) => result,
        Err(error) => {
            reject_waiters(pending, error.to_string());
            return Err(error);
        }
    };
    settle(runtime, call, ending, result, context, Some(duration_ms)).await?;
    resolve_waiters(pending);
    Ok(())
}

fn error_result(message: &str) -> ToolExecutionResult {
    ToolExecutionResult {
        is_error: Some(true),
        diagnostics: Some(vec![tool_diagnostic("tool_error", message)]),
        ..Default::default()
    }
}

fn resolve_waiters(waiters: Vec<crate::harness::output::ProgressWaiter>) {
    for waiter in waiters {
        waiter.resolve();
    }
}

fn reject_waiters(waiters: Vec<crate::harness::output::ProgressWaiter>, error: String) {
    for waiter in waiters {
        waiter.reject(error.clone());
    }
}

/// Throttled commits of what the tool reported into its `pi.live.tools` slot,
/// each writing only what changed since the last one.
fn publish_progress(
    runtime: &dyn TaskRuntime,
    reported: Arc<Mutex<Reported>>,
    context: &Context,
) -> crate::harness::output::Progress {
    let shared = runtime.handle();
    let settings = runtime.settings();
    let written = Arc::new(Mutex::new(WrittenReport {
        text: String::new(),
        details: None,
        diagnostics: 0,
    }));
    let context = context.clone();
    let write_shared = Arc::clone(&shared);
    let write_reported = Arc::clone(&reported);
    let write_written = Arc::clone(&written);
    let error_shared = Arc::clone(&shared);
    crate::harness::output::Progress::new(
        move || {
            let shared = Arc::clone(&write_shared);
            let reported = Arc::clone(&write_reported);
            let written = Arc::clone(&write_written);
            let context = context.clone();
            Box::pin(async move { flush_reported(shared, context, reported, written).await })
        },
        move |error| {
            if !error_shared.signal().is_cancelled() {
                let _ = error_shared.report(error);
            }
        },
        settings.progress.output_interval_ms,
    )
}

async fn flush_reported(
    runtime: Arc<dyn TaskRuntime>,
    context: Context,
    reported: Arc<Mutex<Reported>>,
    written: Arc<Mutex<WrittenReport>>,
) -> Result<u64, String> {
    // Capture everything synchronously: the tool keeps reporting while the
    // commit is in flight.
    let (text, dropped_bytes, dropped_lines, diagnostics, details) = {
        let mut guard = reported.lock().map_err(|_| "tool state".to_string())?;
        let snapshot = guard.output.snapshot();
        (
            snapshot.text,
            snapshot.dropped_bytes,
            snapshot.dropped_lines,
            guard.diagnostics.clone(),
            guard.details.clone(),
        )
    };
    let (previous_text, previous_details, previous_count) = {
        let guard = written.lock().map_err(|_| "progress state".to_string())?;
        (guard.text.clone(), guard.details.clone(), guard.diagnostics)
    };
    let added: Vec<ToolDiagnostic> = diagnostics[previous_count.min(diagnostics.len())..].to_vec();
    let details_changed = details != previous_details;
    // What the commit writes as a Chord diff of the string: an append, or the
    // whole window when the retained tail slid.
    let mut bytes: u64 = 0;
    if text != previous_text {
        let shared = if text.starts_with(&previous_text) {
            previous_text.len()
        } else {
            0
        };
        bytes += (text.len() - shared) as u64;
    }
    if details_changed {
        bytes += serde_json::to_string(&details.clone().unwrap_or(JsonValue::Null))
            .map(|text| text.len() as u64)
            .unwrap_or(0);
    }
    if !added.is_empty() {
        bytes += serde_json::to_string(&added)
            .map(|text| text.len() as u64)
            .unwrap_or(0);
    }
    let task_id = runtime.task_id();
    let conversation_id = runtime.conversation_id();
    let commit_text = text.clone();
    let commit_details = details.clone();
    let commit_added = added;
    let commit: TaskCommitFn = Arc::new(move |tx, _current| {
        let text = commit_text.clone();
        let details = commit_details.clone();
        let added = commit_added.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            if let Some(slots) = state.tools.as_mut()
                && let Some(slot) = slots.iter_mut().find(|slot| slot.task_id == Some(task_id))
            {
                if slot.output.as_deref() != Some(text.as_str()) {
                    slot.output = Some(text);
                }
                if dropped_bytes > 0 {
                    slot.dropped_bytes = Some(dropped_bytes);
                }
                if dropped_lines > 0 {
                    slot.dropped_lines = Some(dropped_lines);
                }
                if details_changed && details.is_some() {
                    slot.details = details;
                }
                if !added.is_empty() {
                    let list = slot.diagnostics.get_or_insert_with(Vec::new);
                    list.extend(added);
                }
            }
            crate::harness::types::write_state(&live, &state)?;
            Ok(None)
        })
    });
    runtime
        .commit(commit, &context)
        .await
        .map_err(|error| error.to_string())?;
    {
        let mut guard = written.lock().map_err(|_| "progress state".to_string())?;
        guard.text = text;
        guard.details = details;
        guard.diagnostics = diagnostics.len();
    }
    Ok(bytes)
}

/// The settled result: the tool's result with the retained output and last
/// details as fallbacks, its diagnostics after those reported through the api,
/// `afterTool` applied, and explicit text bounded, with the Harness's
/// truncation diagnostic last.
async fn final_result(
    runtime: &dyn TaskRuntime,
    call: &ToolCall,
    result: ToolExecutionResult,
    reported: &Arc<Mutex<Reported>>,
    context: &Context,
) -> SessionResult<ToolExecutionResult> {
    let limits = reported
        .lock()
        .map(|guard| guard.limits)
        .unwrap_or(OutputLimitsSpec {
            max_bytes: crate::truncate::DEFAULT_MAX_BYTES,
            max_lines: crate::truncate::DEFAULT_MAX_LINES,
            retain: crate::harness::types::OutputRetain::Head,
        });
    let (retained, diagnostics, details) = {
        let mut guard = reported
            .lock()
            .map_err(|_| SessionError::message("tool state"))?;
        let retained = if result.content.is_none() {
            Some(guard.output.snapshot())
        } else {
            None
        };
        (retained, guard.diagnostics.clone(), guard.details.clone())
    };
    let content = match &retained {
        Some(retained) => {
            if retained.text.is_empty() {
                Vec::new()
            } else {
                vec![ToolResultContent::Text(TextContent {
                    text: retained.text.clone(),
                    text_signature: None,
                })]
            }
        }
        None => result.content.clone().unwrap_or_default(),
    };
    let mut final_result = ToolExecutionResult {
        content: Some(content.clone()),
        is_error: result.is_error,
        details: result.details.clone().or(details),
        diagnostics: Some(
            diagnostics
                .into_iter()
                .chain(result.diagnostics.clone().unwrap_or_default())
                .collect(),
        ),
        usage: result.usage.clone(),
        control: result.control.clone(),
    };
    let agent = runtime.agent(context).await?;
    for handlers in agent_hooks(&agent, TOOL_TASK_NAME) {
        if let HookHandlers::Tool(hooks) = handlers {
            let api = crate::harness::types::RuntimeHookApi { runtime };
            if let Some(replaced) = hooks
                .after_tool(call, final_result.clone(), &api, context)
                .await
            {
                final_result = replaced;
            }
        }
    }
    let mut harness: Vec<ToolDiagnostic> = Vec::new();
    // The retained output's truncation applies only while afterTool kept that
    // content.
    if let Some(retained) = &retained
        && retained.dropped_bytes > 0
        && final_result.content.as_ref() == Some(&content)
    {
        harness.push(truncated(retained, Some(limits.retain)));
    }
    let (bounded, dropped_bytes, dropped_lines) =
        bound_content(final_result.content.clone().unwrap_or_default(), limits);
    if dropped_bytes > 0 {
        harness.push(truncated(
            &crate::harness::output::BoundedOutput {
                text: String::new(),
                dropped_bytes,
                dropped_lines,
            },
            Some(limits.retain),
        ));
    }
    final_result.content = Some(bounded);
    let mut all_diagnostics = final_result.diagnostics.clone().unwrap_or_default();
    all_diagnostics.extend(harness);
    final_result.diagnostics = Some(all_diagnostics);
    Ok(final_result)
}

/// The Harness's truncation diagnostic; `retain` is unknown when rebuilt from
/// a slot after recovery.
fn truncated(
    dropped: &crate::harness::output::BoundedOutput,
    retain: Option<crate::harness::types::OutputRetain>,
) -> ToolDiagnostic {
    let kept = match retain {
        Some(crate::harness::types::OutputRetain::Head) => " to its beginning",
        Some(crate::harness::types::OutputRetain::Tail) => " to its end",
        None => "",
    };
    ToolDiagnostic {
        severity: DiagnosticSeverity::Warn,
        code: Some("truncated".to_string()),
        message: format!(
            "Output truncated{kept}: {} lines, {} bytes dropped",
            dropped.dropped_lines, dropped.dropped_bytes
        ),
    }
}

/// Bound the text of result content. When the joined text exceeds the limits,
/// the text items are replaced by one bounded item at the position of the
/// first (head) or last (tail) text item; other content is kept.
fn bound_content(
    content: Vec<ToolResultContent>,
    limits: OutputLimitsSpec,
) -> (Vec<ToolResultContent>, u64, u64) {
    let joined: String = content
        .iter()
        .filter_map(|item| match item {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    let bounded = crate::harness::output::bound_output(
        &joined,
        crate::harness::output::OutputLimits {
            max_bytes: limits.max_bytes,
            max_lines: limits.max_lines,
            retain: limits.retain,
        },
    );
    if bounded.dropped_bytes == 0 {
        return (content, 0, 0);
    }
    let keep_index = match limits.retain {
        crate::harness::types::OutputRetain::Head => content
            .iter()
            .position(|item| matches!(item, ToolResultContent::Text(_))),
        crate::harness::types::OutputRetain::Tail => content
            .iter()
            .rposition(|item| matches!(item, ToolResultContent::Text(_))),
    };
    let mut result = Vec::with_capacity(content.len());
    for (index, item) in content.into_iter().enumerate() {
        match item {
            ToolResultContent::Text(text) if Some(index) == keep_index => {
                result.push(ToolResultContent::Text(TextContent {
                    text: bounded.text.clone(),
                    text_signature: text.text_signature,
                }));
            }
            ToolResultContent::Text(_) => {}
            other => result.push(other),
        }
    }
    (result, bounded.dropped_bytes, bounded.dropped_lines)
}

/// Append the result entry, mark the slot done, and complete (or end aborted/failed).
async fn settle(
    runtime: &dyn TaskRuntime,
    call: &ToolCall,
    ending: Ending,
    result: ToolExecutionResult,
    context: &Context,
    duration_ms: Option<u64>,
) -> SessionResult<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let now = runtime.now()? as i64;
    let call = call.clone();
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let call = call.clone();
        let result = result.clone();
        let ending = ending.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let entry =
                append_tool_result(tx, conversation_id, &call, &result, now, duration_ms).await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            if let Some(slots) = state.tools.as_mut()
                && let Some(slot) = slots
                    .iter_mut()
                    .find(|slot| slot.task_id == Some(task_id) || slot.call_id == call.id)
            {
                finish_slot(slot, Some(entry.id));
            }
            crate::harness::types::write_state(&live, &state)?;
            let entry_id = entry.id;
            let outcome = match ending {
                Ending::Completed => {
                    let result = match result.control {
                        Some(control) => serde_json::json!({
                            "entryId": entry_id,
                            "control": control,
                        }),
                        None => serde_json::json!({ "entryId": entry_id }),
                    };
                    TaskOutcome::Completed { result }
                }
                Ending::Aborted => TaskOutcome::Aborted {
                    reason: Some("aborted".to_string()),
                    result: Some(serde_json::json!({ "entryId": entry_id })),
                },
                Ending::Failed(message) => TaskOutcome::Failed {
                    error: TaskOutcomeError {
                        message,
                        detail: None,
                    },
                    result: Some(serde_json::json!({ "entryId": entry_id })),
                },
            };
            Ok(Some(NextTaskState::Terminal { outcome }))
        })
    });
    runtime.commit(change, context).await
}

async fn clear_slot(runtime: &dyn TaskRuntime, context: &Context) -> SessionResult<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            if let Some(slots) = state.tools.as_mut()
                && let Some(slot) = slots.iter_mut().find(|slot| slot.task_id == Some(task_id))
            {
                crate::harness::live::clear_progress(slot);
            }
            crate::harness::types::write_state(&live, &state)?;
            Ok(None)
        })
    });
    runtime.commit(change, context).await
}

// ---------------------------------------------------------------------------
// Reading calls and slots
// ---------------------------------------------------------------------------

async fn read_call(
    runtime: &dyn TaskRuntime,
    input: &ToolTaskInput,
    context: &Context,
) -> SessionResult<ToolCall> {
    let entry = runtime
        .entry(input.assistant, context)
        .await?
        .ok_or_else(|| {
            SessionError::message(format!(
                "Entry {} has no tool call {}",
                input.assistant, input.call_id
            ))
        })?;
    let call = entry
        .model
        .and_then(|messages| {
            messages.into_iter().find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
        })
        .and_then(|assistant| {
            assistant
                .content
                .into_iter()
                .find_map(|content| match content {
                    AssistantContent::ToolCall(call) if call.id == input.call_id => Some(call),
                    _ => None,
                })
        });
    call.ok_or_else(|| {
        SessionError::message(format!(
            "Entry {} has no tool call {}",
            input.assistant, input.call_id
        ))
    })
}

async fn read_slot(
    runtime: &dyn TaskRuntime,
    context: &Context,
) -> SessionResult<Option<ToolSlot>> {
    let live = runtime
        .snapshot(
            &LIVE_DOC,
            DocOwner::Conversation(runtime.conversation_id()),
            context,
        )
        .await?;
    let task_id = runtime.task_id();
    let state =
        live.and_then(|value| serde_json::from_value::<LiveState>(JsonValue::Object(value)).ok());
    Ok(state
        .and_then(|state| state.tools)
        .and_then(|slots| slots.into_iter().find(|slot| slot.task_id == Some(task_id))))
}

/// Arguments, or why they are invalid.
fn prepare(tool: &ToolRegistration, args: JsonObject) -> Result<JsonObject, String> {
    let Some(prepare) = &tool.prepare_arguments else {
        return Ok(args);
    };
    let repaired = prepare(&JsonValue::Object(args))?;
    match repaired {
        JsonValue::Object(object) => Ok(object),
        _ => Err("prepareArguments must return an object".to_string()),
    }
}

/// Arguments validated and coerced against the implementation's schema.
fn validate(
    tool: &ToolRegistration,
    call: &ToolCall,
    args: JsonObject,
) -> Result<JsonObject, String> {
    let mut candidate = call.clone();
    candidate.arguments = args;
    match validate_tool_arguments(&tool.tool, &candidate)? {
        JsonValue::Object(object) => Ok(object),
        _ => Err("Validated arguments must be an object".to_string()),
    }
}

fn invalid(message: &str) -> ToolExecutionResult {
    harness_error("invalid_arguments", message)
}

// ---------------------------------------------------------------------------
// Result construction
// ---------------------------------------------------------------------------

/// An error result the Harness writes itself: no content and one diagnostic.
pub fn harness_error(code: &str, message: &str) -> ToolExecutionResult {
    ToolExecutionResult {
        content: Some(Vec::new()),
        is_error: Some(true),
        diagnostics: Some(vec![tool_diagnostic(code, message)]),
        ..Default::default()
    }
}

fn tool_diagnostic(code: &str, message: &str) -> ToolDiagnostic {
    ToolDiagnostic {
        severity: DiagnosticSeverity::Error,
        message: message.to_string(),
        code: Some(code.to_string()),
    }
}

/// An error result from the slot's durable partial output, details, and diagnostics.
fn from_slot(slot: Option<&ToolSlot>, code: &str, message: &str) -> ToolExecutionResult {
    let mut diagnostics = slot
        .and_then(|slot| slot.diagnostics.clone())
        .unwrap_or_default();
    diagnostics.push(tool_diagnostic(code, message));
    let content = slot
        .and_then(|slot| slot.output.clone())
        .filter(|output| !output.is_empty())
        .map(|output| {
            vec![ToolResultContent::Text(TextContent {
                text: output,
                text_signature: None,
            })]
        })
        .unwrap_or_default();
    ToolExecutionResult {
        content: Some(content),
        is_error: Some(true),
        details: slot.and_then(|slot| slot.details.clone()),
        diagnostics: Some(diagnostics),
        ..Default::default()
    }
}

/// Append a `pi.tool-result` entry. The content ends with the rendered
/// diagnostics, so the stored message is exactly what the model sees; `data`
/// keeps the structured list. A result's usage is added to `pi.usage` in the
/// same commit.
pub async fn append_tool_result(
    tx: &mut Transaction,
    conversation_id: ConversationId,
    call: &ToolCall,
    result: &ToolExecutionResult,
    timestamp: i64,
    duration_ms: Option<u64>,
) -> SessionResult<EntryRecord> {
    let diagnostics = result.diagnostics.clone().unwrap_or_default();
    let mut content = result.content.clone().unwrap_or_default();
    if !diagnostics.is_empty() {
        content.push(ToolResultContent::Text(TextContent {
            text: render_diagnostics(&diagnostics),
            text_signature: None,
        }));
    }
    let message = ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content,
        details: result.details.clone(),
        usage: result.usage.clone(),
        nested_calls: None,
        is_error: result.is_error.unwrap_or(false),
        timestamp,
        duration_ms,
    };
    if let Some(usage) = &result.usage {
        record_usage(tx, conversation_id, UsageBucket::Tools, &call.name, usage).await?;
    }
    let data = serde_json::json!({ "diagnostics": diagnostics });
    let mut draft = EntryDraft::new(TOOL_RESULT_ENTRY);
    draft.model = Some(vec![Message::ToolResult(message)]);
    draft.data = Some(data);
    tx.append_entry(conversation_id, draft).await
}

fn render_diagnostics(diagnostics: &[ToolDiagnostic]) -> String {
    let body = diagnostics
        .iter()
        .map(|diagnostic| {
            let severity = match diagnostic.severity {
                DiagnosticSeverity::Info => "info",
                DiagnosticSeverity::Warn => "warn",
                DiagnosticSeverity::Error => "error",
            };
            format!("[{severity}] {}", diagnostic.message)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("<harness>\n{body}\n</harness>")
}

// ---------------------------------------------------------------------------
// Tool execution API
// ---------------------------------------------------------------------------

/// [`ToolExecutionApi`] backed by the tool task's invocation.
struct ToolApi {
    runtime: Arc<dyn TaskRuntime>,
    call_id: String,
    env: Option<Arc<dyn crate::env::types::ExecutionEnv>>,
    reported: Arc<Mutex<Reported>>,
    progress: crate::harness::output::Progress,
    ended: std::sync::atomic::AtomicBool,
}

impl ToolApi {
    fn new(
        runtime: &dyn TaskRuntime,
        call_id: String,
        reported: Arc<Mutex<Reported>>,
        env: Option<Arc<dyn crate::env::types::ExecutionEnv>>,
        progress: crate::harness::output::Progress,
    ) -> Self {
        Self {
            runtime: runtime.handle(),
            call_id,
            env,
            reported,
            progress,
            ended: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Mark the call settled: later reports fail like upstream's `assertLive`.
    fn mark_ended(&self) {
        self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn assert_live(&self) -> Result<(), String> {
        if self.ended.load(std::sync::atomic::Ordering::SeqCst) {
            Err(format!("Tool call {} has settled", self.call_id))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl ToolExecutionApi for ToolApi {
    fn task_id(&self) -> TaskId {
        self.runtime.task_id()
    }

    fn conversation_id(&self) -> ConversationId {
        self.runtime.conversation_id()
    }

    fn call_id(&self) -> &str {
        &self.call_id
    }

    fn registry(&self) -> Arc<dyn RegistrySnapshot> {
        self.runtime.registry()
    }

    async fn agent(&self, context: &Context) -> Result<Agent, SessionError> {
        self.runtime.agent(context).await
    }

    fn models(&self) -> &Models {
        self.runtime.models()
    }

    fn env(&self) -> Option<Arc<dyn crate::env::types::ExecutionEnv>> {
        self.env.clone()
    }

    fn output(
        &self,
        chunk: &[u8],
        skipped: Option<crate::harness::types::OutputSkip>,
    ) -> Result<(), String> {
        self.assert_live()?;
        let accepted = {
            let mut reported = self.reported.lock().map_err(|_| "tool state".to_string())?;
            reported
                .output
                .push(crate::harness::output::OutputChunk::Bytes(chunk), skipped)
                .map_err(|error| error.to_string())?
        };
        if accepted {
            self.progress().mark();
        }
        Ok(())
    }

    fn output_window(&self) -> Option<crate::harness::types::OutputWindow> {
        let reported = self.reported.lock().ok()?;
        if reported.limits.retain != crate::harness::types::OutputRetain::Tail {
            return None;
        }
        Some(crate::harness::types::OutputWindow {
            max_bytes: reported.limits.max_bytes,
            max_lines: reported.limits.max_lines,
            min_interval_ms: self.runtime.settings().progress.output_interval_ms,
            bytes_per_second: crate::harness::output::PROGRESS_BYTES_PER_SECOND,
        })
    }

    fn diagnostic(&self, diagnostic: ToolDiagnostic) {
        if self.assert_live().is_err() {
            return;
        }
        if let Ok(mut reported) = self.reported.lock() {
            reported.diagnostics.push(diagnostic);
        }
        self.progress().mark();
    }

    async fn details(&self, value: JsonValue, context: &Context) -> Result<(), SessionError> {
        self.assert_live().map_err(SessionError::message)?;
        if context
            .abort_signal()
            .map(|signal| signal.is_cancelled())
            .unwrap_or(false)
        {
            return Err(SessionError::Aborted);
        }
        if let Ok(mut reported) = self.reported.lock() {
            reported.details = Some(value);
        }
        let receiver = self.progress().mark_and_wait();
        // Cancelling the wait leaves the update in place; the commit's own
        // outcome stays observed.
        match crate::chord::context::await_with_context(
            async { receiver.await.map_err(|_| SessionError::Aborted) },
            context,
        )
        .await
        {
            Ok(result) => result?.map_err(SessionError::message),
            Err(_) => Err(SessionError::Aborted),
        }
    }

    async fn commit(
        &self,
        change: ToolCommitFn,
        context: &Context,
    ) -> Result<JsonValue, SessionError> {
        let holder: Arc<Mutex<Option<JsonValue>>> = Arc::new(Mutex::new(None));
        let target = Arc::clone(&holder);
        let wrapper: TaskCommitFn = Arc::new(move |tx, _current| {
            let change = Arc::clone(&change);
            let target = Arc::clone(&target);
            Box::pin(async move {
                let value = change(tx).await?;
                if let Ok(mut slot) = target.lock() {
                    *slot = Some(value);
                }
                Ok(None)
            })
        });
        self.runtime.commit(wrapper, context).await?;
        match holder.lock() {
            Ok(mut slot) => Ok(slot.take().unwrap_or(JsonValue::Null)),
            Err(_) => Err(SessionError::message("Tool commit result unavailable")),
        }
    }

    async fn memo(&self, name: &str, context: &Context) -> Result<Option<JsonValue>, SessionError> {
        self.runtime.memo(name, context).await
    }

    async fn memo_set(
        &self,
        name: &str,
        value: JsonValue,
        context: &Context,
    ) -> Result<JsonValue, SessionError> {
        self.runtime.memo_set(name, value, context).await
    }

    async fn get_task(
        &self,
        id: TaskId,
        context: &Context,
    ) -> Result<Option<TaskRecord>, SessionError> {
        self.runtime.get_task(id, context).await
    }

    async fn wait_for_task(
        &self,
        id: TaskId,
        context: &Context,
    ) -> Result<SettledTask<JsonValue>, SessionError> {
        self.runtime.wait_for_task(id, context).await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> Result<Option<Arc<dyn ConversationHandle>>, SessionError> {
        self.runtime.conversation(id, context).await
    }
}

impl ToolApi {
    /// The progress pump shared with the run loop.
    fn progress(&self) -> &crate::harness::output::Progress {
        &self.progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_error_has_one_diagnostic_and_error_flag() {
        let result = harness_error("tool_unavailable", "nope");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.content.as_ref().map(Vec::len), Some(0));
        assert_eq!(result.diagnostics.as_ref().map(Vec::len), Some(1));
        assert_eq!(
            result.diagnostics.as_ref().and_then(|d| d[0].code.clone()),
            Some("tool_unavailable".to_string())
        );
    }

    #[test]
    fn render_diagnostics_wraps_in_harness() {
        let rendered = render_diagnostics(&[tool_diagnostic("blocked", "no")]);
        assert!(rendered.starts_with("<harness>\n"));
        assert!(rendered.contains("[error] no"));
    }
}
