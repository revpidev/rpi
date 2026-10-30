//! Built-in tool task (port of `packages/durable/src/harness/tool.ts`
//! @ `4a080252f`).
//!
//! A tool task resolves the called tool among its phase agent's tools, validates
//! the arguments, records the intent, executes the tool, and appends the result
//! entry, all in one `call` handler. The `execute` phase is reached only by
//! recovery and applies the replay rule.
//!
//! A model-issued call and a nested call (`executeTool()`) share the task: the
//! input is a tagged union, the nested call's result lives in its caller's
//! `NestedCallDoc`, and both publish progress in `pi.live`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

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
use crate::documents::{DocDefinition, DocScope, DocToken, define_doc_family};
use crate::entries::TOOL_RESULT_ENTRY;
use crate::harness::agent::agent_hooks;
use crate::harness::live::{
    LIVE_DOC, LiveState, NestedToolSlot, NestedToolSummary, SlotSnapshot, ToolSlotStatus,
    clear_progress, remove_nested_slots, tool_slot_mut,
};
use crate::harness::registry::TOOL_TASK_NAME;
use crate::harness::types::{
    Agent, ConversationHandle, DiagnosticSeverity, HookHandlers, NextTaskState, OutputLimits,
    OutputRetain, RegistrySnapshot, RunningTask, SettledTask, TaskCommitFn, TaskDefinition,
    TaskRuntime, ToolCommitFn, ToolDiagnostic, ToolExecutionApi, ToolExecutionResult, ToolHookCall,
    ToolRegistration, ToolReplay,
};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::ids::{ConversationId, EntryId, TaskId};
use crate::session::{
    DocOwner, EntryDraft, SessionError, SessionResult, TaskCreate, TaskOwnership, Transaction,
};
use crate::types::{TaskOutcome, TaskOutcomeError, TaskRecord, TaskState};

/// A model-issued call, read from its assistant entry, or a nested call a
/// running tool made through `executeTool()`, whose call the input carries
/// because no entry holds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
// The upstream union also carries the whole call in its nested arm.
#[allow(clippy::large_enum_variant)]
pub enum ToolTaskInput {
    /// The assistant entry and the provider call ID.
    Model {
        /// The tool-calling answer.
        assistant: EntryId,
        /// Provider call ID.
        call_id: String,
    },
    /// A nested call of a running tool.
    Nested {
        /// Calling tool task.
        parent: TaskId,
        /// Call ID of the calling tool.
        parent_call_id: String,
        /// Key within the calling tool; its call ID is `<parentCallId>/<key>`.
        key: String,
        /// The call, since no entry holds it.
        call: ToolCall,
        /// `false`: commit no running output, details, or diagnostics to the
        /// slot; the result still gets them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<bool>,
    },
}

impl ToolTaskInput {
    /// Whether this is a nested call.
    pub fn is_nested(&self) -> bool {
        matches!(self, ToolTaskInput::Nested { .. })
    }
}

/// Durable tool checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum ToolCheckpoint {
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

/// A model-issued call's result is its transcript entry; a nested call's result
/// is in its caller's `NestedCallDoc`.
///
/// A version 1 task that was already holding its outcome as `completing` when
/// the Harness upgraded finishes with a version 1 result, `{ entryId, control? }`
/// without `kind`; a result without `kind` is a model-issued call's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ToolTaskResult {
    /// Model-issued call: the appended entry and post-tools controls.
    Model {
        /// Appended result entry.
        entry_id: EntryId,
        /// Post-tools controls, when the tool returned any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<crate::harness::types::ToolControl>,
    },
    /// Nested call: the result lives in the caller's `NestedCallDoc`.
    Nested,
}

/// One nested call of a running tool: its task, and once it ended, its result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedCallState {
    /// The nested call's tool task; absent only inside the commit that creates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Exactly what `executeTool()` returns; absent while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<crate::harness::types::NestedToolExecutionResult>,
}

fn nested_call_initial(_seed: Option<&JsonValue>) -> JsonObject {
    JsonObject::new()
}

/// The nested calls of a tool call, a task-scoped family of the calling task
/// keyed by the nested call's key.
pub static NESTED_CALL_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    define_doc_family(DocDefinition {
        kind: "pi.tool.nested-call".to_string(),
        version: 1,
        scope: DocScope::Task,
        history: None,
        fork: None,
        family: true,
        initial: Arc::new(nested_call_initial),
        migrate: None,
        // The document retires with its task.
        checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
    })
});

/// Memo of a tool task that made a nested call, in any attempt, committed
/// before the first admission of each attempt.
const MADE_NESTED_CALLS: &str = "pi.tool.madeNestedCalls";

/// Builds the terminal result from the durable slot (or without one) inside
/// the settling commit.
pub type ToolResultBuilder =
    Arc<dyn Fn(Option<&SlotSnapshot>) -> ToolExecutionResult + Send + Sync>;

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
        2
    }

    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "call" })
    }

    fn migrate(
        &self,
        input: JsonValue,
        checkpoint: JsonValue,
        from_version: u64,
    ) -> Option<crate::harness::types::Migration> {
        if from_version >= 2 {
            return None;
        }
        // Version 1 had only model-issued calls.
        let assistant = input.get("assistant").cloned()?;
        let call_id = input.get("callId").cloned()?;
        Some(crate::harness::types::Migration {
            input: serde_json::json!({
                "kind": "model",
                "assistant": assistant,
                "callId": call_id,
            }),
            checkpoint,
        })
    }

    fn has_migration(&self) -> bool {
        true
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
        let message = format!("Tool {} was aborted", call.call.name);
        let result = from_slot(
            read_slot(runtime, context).await?.as_ref(),
            "aborted",
            &message,
        );
        settle(
            runtime,
            &input,
            &call,
            Ending::Aborted,
            {
                let owned = result.clone();
                Arc::new(move |_| owned.clone())
            },
            context,
            None,
        )
        .await
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
    let Some(tool) = resolve_tool(runtime, &input, &call.call.name, context).await? else {
        let error = harness_error(
            "tool_unavailable",
            &format!("Tool {} is not available", call.call.name),
        );
        return settle(
            runtime,
            &input,
            &call,
            Ending::Completed,
            {
                let owned = error.clone();
                Arc::new(move |_| owned.clone())
            },
            context,
            None,
        )
        .await;
    };
    let prepared = match prepare(&tool, call.call.arguments.clone()) {
        Err(error) => Err(error),
        Ok(args) => validate(&tool, &call.call, args),
    };
    let mut args = match prepared {
        Err(error) => {
            let result = invalid(&error);
            return settle(
                runtime,
                &input,
                &call,
                Ending::Completed,
                {
                    let owned = result.clone();
                    Arc::new(move |_| owned.clone())
                },
                context,
                None,
            )
            .await;
        }
        Ok(args) => args,
    };
    let agent = runtime.agent(context).await?;
    let mut block: Option<String> = None;
    for handlers in agent_hooks(&agent, TOOL_TASK_NAME) {
        if block.is_some() {
            break;
        }
        if let HookHandlers::Tool(hooks) = handlers {
            let api = crate::harness::types::RuntimeHookApi { runtime };
            let mut call_with_args = call.clone();
            call_with_args.call.arguments = args.clone();
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
        return settle(
            runtime,
            &input,
            &call,
            Ending::Completed,
            {
                let owned = result.clone();
                Arc::new(move |_| owned.clone())
            },
            context,
            None,
        )
        .await;
    }
    let final_args = match validate(&tool, &call.call, args) {
        Ok(args) => args,
        Err(error) => {
            let result = invalid(&error);
            return settle(
                runtime,
                &input,
                &call,
                Ending::Completed,
                {
                    let owned = result.clone();
                    Arc::new(move |_| owned.clone())
                },
                context,
                None,
            )
            .await;
        }
    };
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let replay = match tool.replay {
        ToolReplay::Safe => "safe",
        ToolReplay::Unsafe => "unsafe",
    }
    .to_string();
    let intent_args = final_args.clone();
    let input_for_commit = input.clone();
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let intent_args = intent_args.clone();
        let replay = replay.clone();
        let input_for_commit = input_for_commit.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            if let Some(mut slot) = tool_slot_mut(&mut state, task_id) {
                slot.progress().status = ToolSlotStatus::Running;
                // A nested slot shows the arguments the call runs with, which
                // repair, hooks, and coercion may have changed.
                if let ToolTaskInput::Nested { call, .. } = &input_for_commit
                    && !crate::harness::json_equal(
                        &JsonValue::Object(call.arguments.clone()),
                        &JsonValue::Object(intent_args.clone()),
                    )
                    && let crate::harness::live::LiveSlot::Nested(slot) = slot
                {
                    slot.arguments = intent_args.clone();
                }
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
    run(runtime, &input, &call, &tool, final_args, context).await
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
    let tool = resolve_tool(runtime, &input, &call.call.name, context).await?;
    if replay == "safe"
        && let Some(tool) = &tool
        && tool.replay == ToolReplay::Safe
    {
        // The rerun reports from scratch; clear what the interrupted attempt
        // published.
        clear_slot(runtime, context).await?;
        return run(runtime, &input, &call, tool, args, context).await;
    }
    let message = format!(
        "Tool {} was interrupted and may have partially run",
        call.call.name
    );
    let result = from_slot(
        read_slot(runtime, context).await?.as_ref(),
        "interrupted",
        &message,
    );
    settle(
        runtime,
        &input,
        &call,
        Ending::Failed(message),
        {
            let owned = result.clone();
            Arc::new(move |_| owned.clone())
        },
        context,
        None,
    )
    .await
}

/// The called tool: among the tools the model is offered for a model-issued
/// call, else among those tools may call.
async fn resolve_tool(
    runtime: &dyn TaskRuntime,
    input: &ToolTaskInput,
    name: &str,
    context: &Context,
) -> SessionResult<Option<ToolRegistration>> {
    let agent: Agent = runtime.agent(context).await?;
    let tools = match input {
        ToolTaskInput::Model { .. } => &agent.tools,
        ToolTaskInput::Nested { .. } => &agent.callable,
    };
    Ok(tools.iter().find(|tool| tool.name() == name).cloned())
}

/// Resolved output bounds of one tool call.
#[derive(Debug, Clone, Copy)]
struct OutputLimitsSpec {
    max_bytes: u64,
    max_lines: u64,
    retain: OutputRetain,
}

fn resolve_output_limits(limits: Option<OutputLimits>) -> OutputLimitsSpec {
    OutputLimitsSpec {
        max_bytes: limits
            .and_then(|limits| limits.max_bytes)
            .unwrap_or(crate::truncate::DEFAULT_MAX_BYTES),
        max_lines: limits
            .and_then(|limits| limits.max_lines)
            .unwrap_or(crate::truncate::DEFAULT_MAX_LINES),
        retain: limits
            .and_then(|limits| limits.retain)
            .unwrap_or(OutputRetain::Head),
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

/// Tracks nested-call admissions a tool started but has not finished admitting;
/// cleanup waits for them so it sees every nested call.
struct AdmissionTracker {
    state: Mutex<(bool, usize)>,
    notify: tokio::sync::Notify,
}

impl AdmissionTracker {
    fn new() -> Self {
        Self {
            state: Mutex::new((false, 0)),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn is_ended(&self) -> bool {
        self.state.lock().map(|state| state.0).unwrap_or(true)
    }

    /// Register one admission; fails once the call settled.
    fn begin(self: &Arc<Self>) -> Result<Admission, String> {
        let mut state = self.state.lock().map_err(|_| "tool state".to_string())?;
        if state.0 {
            return Err("tool call has settled".to_string());
        }
        state.1 += 1;
        Ok(Admission {
            tracker: Arc::clone(self),
        })
    }

    /// No admission starts after this; later `begin` calls fail.
    fn end(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.0 = true;
        }
    }

    /// Wait until every registered admission finished.
    async fn drain(&self) {
        loop {
            let notified = self.notify.notified();
            let active = self.state.lock().map(|state| state.1).unwrap_or(0);
            if active == 0 {
                return;
            }
            notified.await;
        }
    }
}

struct Admission {
    tracker: Arc<AdmissionTracker>,
}

impl Drop for Admission {
    fn drop(&mut self) {
        if let Ok(mut state) = self.tracker.state.lock() {
            state.1 = state.1.saturating_sub(1);
        }
        self.tracker.notify.notify_waiters();
    }
}

/// Execute with the resolved implementation, then settle its result.
async fn run(
    runtime: &dyn TaskRuntime,
    input: &ToolTaskInput,
    call: &ToolHookCall,
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
    let admissions = Arc::new(AdmissionTracker::new());
    let streams = match input {
        ToolTaskInput::Model { .. } => true,
        ToolTaskInput::Nested { progress, .. } => *progress != Some(false),
    };
    let progress = publish_progress(runtime, Arc::clone(&reported), streams, context);
    let resumes = tool.replay == ToolReplay::Safe;
    let sequence = Arc::new(AtomicUsize::new(0));
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
        call.call.id.clone(),
        Arc::clone(&reported),
        env,
        progress.clone(),
        Arc::clone(&admissions),
        sequence,
        resumes,
    );

    let started = std::time::Instant::now();
    let mut ending = Ending::Completed;
    let result = if let Some(message) = env_error {
        ending = Ending::Failed(format!("Tool {} threw", call.call.name));
        error_result(&message)
    } else {
        match tool.execute.execute(args, &api, context).await {
            Ok(result) => result,
            Err(error) => {
                if runtime.signal().is_cancelled() {
                    admissions.end();
                    let pending = progress.stop().await;
                    reject_waiters(pending, error.to_string());
                    return Err(SessionError::Aborted);
                }
                ending = Ending::Failed(format!("Tool {} threw", call.call.name));
                runtime.report_info(
                    crate::harness::types::ReportInfo::new(
                        crate::harness::types::ReportKind::TaskError,
                    )
                    .with_task(runtime.task_id(), TOOL_TASK_NAME)
                    .with_reason("threw"),
                );
                error_result(&error.to_string())
            }
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    admissions.end();
    // No admission starts after `ended`; let those underway commit, so
    // settlement aborts and lists their calls.
    admissions.drain().await;
    if let Ok(mut reported) = reported.lock() {
        reported.output.end();
    }
    // Details still waiting for a progress commit settle with the terminal
    // commit, the final flush.
    let pending = progress.stop().await;
    let settled = final_result(runtime, input, call, tool, result, &reported, context).await;
    let result = match settled {
        Ok(result) => result,
        Err(error) => {
            reject_waiters(pending, error.to_string());
            return Err(error);
        }
    };
    settle(
        runtime,
        input,
        call,
        ending,
        {
            let result = result.clone();
            Arc::new(move |slot: Option<&SlotSnapshot>| match slot {
                Some(slot) => apply_result_to_slot(slot, &result),
                None => result.clone(),
            })
        },
        context,
        Some(duration_ms),
    )
    .await?;
    resolve_waiters(pending);
    Ok(())
}

/// The result with the durable slot's partial output merged in (settling
/// `build` runs inside the terminal commit; the result is already complete, so
/// this only matters for error paths built from the slot).
fn apply_result_to_slot(_slot: &SlotSnapshot, result: &ToolExecutionResult) -> ToolExecutionResult {
    result.clone()
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

/// Throttled commits of what the tool reported into its `pi.live` slot, each
/// writing only what changed since the last one.
fn publish_progress(
    runtime: &dyn TaskRuntime,
    reported: Arc<Mutex<Reported>>,
    enabled: bool,
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
    let now_runtime = Arc::clone(&shared);
    let timer_runtime = Arc::clone(&shared);
    let clock = crate::harness::output::ProgressClock {
        now: Arc::new(move || now_runtime.now().unwrap_or(0.0)),
        arm: Arc::new(move |callback, delay_ms| {
            let _ = timer_runtime.set_timer(callback, delay_ms.max(0.0).round() as u64);
        }),
    };
    crate::harness::output::Progress::with_clock(
        move || {
            let shared = Arc::clone(&write_shared);
            let reported = Arc::clone(&write_reported);
            let written = Arc::clone(&write_written);
            let context = context.clone();
            Box::pin(async move {
                if !enabled {
                    return Ok(0);
                }
                flush_reported(shared, context, reported, written).await
            })
        },
        move |error| {
            if !error_shared.signal().is_cancelled() {
                let _ = error_shared.report(error);
            }
        },
        settings.progress.output_interval_ms,
        clock,
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
            if let Some(mut slot) = tool_slot_mut(&mut state, task_id) {
                let progress = slot.progress();
                if progress.output.as_deref() != Some(text.as_str()) {
                    progress.output = Some(text);
                }
                if dropped_bytes > 0 {
                    progress.dropped_bytes = Some(dropped_bytes);
                }
                if dropped_lines > 0 {
                    progress.dropped_lines = Some(dropped_lines);
                }
                if details_changed && details.is_some() {
                    progress.details = details;
                }
                if !added.is_empty() {
                    let list = progress.diagnostics.get_or_insert_with(Vec::new);
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
/// `afterTool` applied, explicit text bounded, and `structuredOutput` checked
/// against the tool's schema.
async fn final_result(
    runtime: &dyn TaskRuntime,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    tool: &ToolRegistration,
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
            retain: OutputRetain::Head,
        });
    let (retained, diagnostics, details) = {
        let mut guard = reported
            .lock()
            .map_err(|_| SessionError::message("tool state"))?;
        let retained = if result.output.is_none() {
            Some(guard.output.snapshot())
        } else {
            None
        };
        (retained, guard.diagnostics.clone(), guard.details.clone())
    };
    let output: Vec<ToolResultContent> = match &retained {
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
        None => result.output.clone().unwrap_or_default(),
    };
    let mut harness: Vec<ToolDiagnostic> = Vec::new();
    let mut final_result = ToolExecutionResult {
        output: Some(output.clone()),
        structured_output: result.structured_output.clone(),
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
    // The retained output's truncation applies only while afterTool kept that
    // output.
    if let Some(retained) = &retained
        && retained.dropped_bytes > 0
        && final_result.output.as_ref() == Some(&output)
    {
        harness.push(truncated(
            &crate::harness::output::BoundedOutput {
                text: String::new(),
                dropped_bytes: retained.dropped_bytes,
                dropped_lines: retained.dropped_lines,
            },
            Some(limits.retain),
        ));
    }
    let (bounded, dropped_bytes, dropped_lines) =
        bound_content(final_result.output.clone().unwrap_or_default(), limits);
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
    final_result.output = Some(bounded.clone());
    if let Some(broken) = structured_output_error(tool, &final_result) {
        final_result.structured_output = None;
        if input.is_nested() {
            final_result.is_error = Some(true);
            harness.push(tool_diagnostic("invalid_structured_output", &broken));
        } else {
            // The model never sees structured output, so its call stands; the
            // host learns of the broken tool.
            let _ = runtime.report(broken);
        }
    }
    // Programs get the tool's own value: without a schema, its output.
    if input.is_nested() && tool.structured_output_schema.is_none() {
        final_result.structured_output = Some(output_value(&bounded));
    }
    // A schema tool's error result without structured output has its text only
    // in the output, which programs never get: it becomes an error diagnostic.
    if input.is_nested()
        && tool.structured_output_schema.is_some()
        && final_result.is_error == Some(true)
        && final_result.structured_output.is_none()
        && !final_result
            .diagnostics
            .iter()
            .flatten()
            .chain(harness.iter())
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
    {
        let text = bounded
            .iter()
            .filter_map(|item| match item {
                ToolResultContent::Text(text) => Some(text.text.as_str()),
                ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            harness.push(tool_diagnostic("tool_error", &text));
        }
    }
    let mut all_diagnostics = final_result.diagnostics.take().unwrap_or_default();
    all_diagnostics.extend(harness);
    final_result.diagnostics = Some(all_diagnostics);
    Ok(final_result)
}

/// Why a result's `structured_output` breaks the tool's contract, or `None`
/// when it keeps it.
fn structured_output_error(
    tool: &ToolRegistration,
    result: &ToolExecutionResult,
) -> Option<String> {
    let schema = tool.structured_output_schema.as_ref();
    let value = result.structured_output.as_ref();
    match (schema, value) {
        (None, None) => None,
        (None, Some(_)) => Some(format!(
            "Tool {} returned structuredOutput but declares no structuredOutputSchema",
            tool.name()
        )),
        (Some(_), None) => {
            if result.is_error == Some(true) {
                None
            } else {
                Some(format!("Tool {} returned no structuredOutput", tool.name()))
            }
        }
        (Some(schema), Some(value)) => {
            let validator = match jsonschema::draft7::new(schema) {
                Ok(validator) => validator,
                Err(error) => {
                    return Some(format!(
                        "Tool {} has an invalid structuredOutputSchema: {error}",
                        tool.name()
                    ));
                }
            };
            if validator.is_valid(value) {
                return None;
            }
            let (path, message) = validator
                .iter_errors(value)
                .next()
                .map(|error| {
                    let path = error.instance_path.to_string();
                    let path = path.trim_start_matches('/').replace('/', ".");
                    let path = if path.is_empty() {
                        "root".to_string()
                    } else {
                        path
                    };
                    (path, error.to_string())
                })
                .unwrap_or_else(|| ("root".to_string(), "invalid".to_string()));
            Some(format!(
                "Tool {} returned structuredOutput that does not match its schema: {path}: {message}",
                tool.name()
            ))
        }
    }
}

/// What programs get of a tool's output when it declares no schema: one text
/// item as its string, one image as itself, nothing as `""`, and anything else
/// as the content list.
fn output_value(output: &[ToolResultContent]) -> JsonValue {
    if output.is_empty() {
        return JsonValue::String(String::new());
    }
    if output.len() > 1 {
        return serde_json::to_value(output).unwrap_or(JsonValue::Array(Vec::new()));
    }
    match &output[0] {
        ToolResultContent::Text(text) => JsonValue::String(text.text.clone()),
        ToolResultContent::Image(image) => serde_json::to_value(image).unwrap_or(JsonValue::Null),
    }
}

/// Commit the tool's terminal state and mark its slot done.
async fn settle(
    runtime: &dyn TaskRuntime,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    ending: Ending,
    build: ToolResultBuilder,
    context: &Context,
    duration_ms: Option<u64>,
) -> SessionResult<()> {
    // A crash in between leaves the call in `execute`: a safe rerun reattaches
    // to the calls, an interruption ends here. Every live nested call the task
    // owns, also those an earlier attempt made and this one never called again.
    let nested_ids: Vec<TaskId> = runtime
        .owned_tasks(context)
        .await?
        .into_iter()
        .filter(|record| {
            record.kind == TOOL_TASK_NAME
                && serde_json::from_value::<ToolTaskInput>(record.input.clone())
                    .map(|input| input.is_nested())
                    .unwrap_or(false)
        })
        .map(|record| record.id)
        .collect();
    // Read before the settling commit: no admission starts after `ended`, and
    // those underway were awaited.
    let made_nested = runtime
        .memo(MADE_NESTED_CALLS, context)
        .await?
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    // Abort each and wait until it is terminal; a nested call may run until
    // it is cancelled itself.
    for id in nested_ids {
        runtime.abort_owned(id, context).await?;
    }
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let input = input.clone();
    let call = call.clone();
    let ending = ending.clone();
    let now = runtime.now()? as i64;
    let build_result = Arc::clone(&build);
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let input = input.clone();
        let call = call.clone();
        let ending = ending.clone();
        let build_result = Arc::clone(&build_result);
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            let snapshot: Option<SlotSnapshot> = {
                let found = state.tools.as_ref().and_then(|tools| {
                    tools
                        .iter()
                        .find(|slot| slot.task_id == Some(task_id))
                        .map(|slot| SlotSnapshot::from(&slot.progress))
                });
                match found {
                    Some(snapshot) => Some(snapshot),
                    None => state
                        .nested_tools
                        .as_ref()
                        .and_then(|nested| nested.iter().find(|slot| slot.task_id == task_id))
                        .map(|slot| SlotSnapshot::from(&slot.progress)),
                }
            };
            let result = build_result(snapshot.as_ref());
            let settled = match &input {
                ToolTaskInput::Nested { parent, key, .. } => {
                    let nested = nested_result(task_id, &result, duration_ms);
                    if let Some(usage) = &nested.usage {
                        record_usage(
                            tx,
                            conversation_id,
                            UsageBucket::Tools,
                            &call.call.name,
                            usage,
                        )
                        .await?;
                    }
                    let draft = tx
                        .doc(
                            &NESTED_CALL_DOC,
                            DocOwner::TaskFamily(*parent, key.as_str()),
                            None,
                        )
                        .await?;
                    let mut nested_state: NestedCallState =
                        crate::harness::types::read_state(&draft)?;
                    if nested_state.task_id != Some(task_id) {
                        return Err(SessionError::message(format!(
                            "Nested call {} is not in its caller's index",
                            call.call.id
                        )));
                    }
                    nested_state.result = Some(nested.clone());
                    crate::harness::types::write_state(&draft, &nested_state)?;
                    if let Some(mut slot) = tool_slot_mut(&mut state, task_id) {
                        slot.finish();
                        if let crate::harness::live::LiveSlot::Nested(slot) = slot {
                            slot.summary = Some(summary_of(
                                &nested,
                                &result.output.clone().unwrap_or_default(),
                            ));
                        }
                    }
                    serde_json::json!({ "kind": "nested" })
                }
                ToolTaskInput::Model { .. } => {
                    let entry = append_tool_result(
                        tx,
                        conversation_id,
                        &call.call,
                        &result,
                        now,
                        duration_ms,
                    )
                    .await?;
                    if let Some(mut slot) = tool_slot_mut(&mut state, task_id) {
                        slot.finish();
                        if let crate::harness::live::LiveSlot::Model(slot) = slot {
                            slot.entry = Some(entry.id);
                        }
                    }
                    let control = match (&result.control, &ending) {
                        (Some(control), Ending::Completed) => {
                            serde_json::to_value(control).unwrap_or(JsonValue::Null)
                        }
                        _ => JsonValue::Null,
                    };
                    if control.is_null() {
                        serde_json::json!({ "kind": "model", "entryId": entry.id })
                    } else {
                        serde_json::json!({
                            "kind": "model",
                            "entryId": entry.id,
                            "control": control,
                        })
                    }
                }
            };
            // A call that made no nested calls, in any attempt, has no slots
            // below it; skipping the scan keeps many leaves linear.
            if made_nested {
                remove_nested_slots(&mut state, task_id);
            }
            crate::harness::types::write_state(&live, &state)?;
            let outcome = match ending {
                Ending::Completed => TaskOutcome::Completed { result: settled },
                Ending::Aborted => TaskOutcome::Aborted {
                    reason: Some("aborted".to_string()),
                    result: Some(settled),
                },
                Ending::Failed(message) => TaskOutcome::Failed {
                    error: TaskOutcomeError {
                        message,
                        detail: None,
                    },
                    result: Some(settled),
                },
            };
            Ok(Some(NextTaskState::Terminal { outcome }))
        })
    });
    runtime.commit(change, context).await
}

/// A nested call's result, as stored and returned: strict JSON, without
/// `output`, which only the model reads, and without `control`.
fn nested_result(
    task_id: TaskId,
    result: &ToolExecutionResult,
    duration_ms: Option<u64>,
) -> crate::harness::types::NestedToolExecutionResult {
    crate::harness::types::NestedToolExecutionResult {
        task_id,
        structured_output: result.structured_output.clone(),
        is_error: result.is_error.unwrap_or(false),
        details: result.details.clone(),
        diagnostics: result.diagnostics.clone().unwrap_or_default(),
        usage: result.usage.clone(),
        duration_ms,
    }
}

/// Create nested call `key` of the call `parent_call_id` in one commit: its tool
/// task, owned by the calling task, its slot, and its `NestedCallDoc`. A key
/// already made returns that call's task when it names the same tool and
/// arguments.
#[allow(clippy::too_many_arguments)]
async fn start_nested_call(
    runtime: &dyn TaskRuntime,
    parent_call_id: &str,
    name: &str,
    args: &JsonObject,
    key: &str,
    progress: Option<bool>,
    abandon_on_restart: bool,
    context: &Context,
) -> SessionResult<TaskId> {
    let call = ToolCall {
        id: format!("{parent_call_id}/{key}"),
        name: name.to_string(),
        arguments: args.clone(),
        arguments_error: None,
        namespace: None,
        thought_signature: None,
    };
    let conversation_id = runtime.conversation_id();
    let parent_task = runtime.task_id();
    let key = key.to_string();
    let call_for_commit = call.clone();
    let parent_call_id = parent_call_id.to_string();
    let created: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
    let replayed: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let created_commit = Arc::clone(&created);
    let replayed_commit = Arc::clone(&replayed);
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let call = call_for_commit.clone();
        let key = key.clone();
        let parent_call_id = parent_call_id.clone();
        let created = Arc::clone(&created_commit);
        let replayed = Arc::clone(&replayed_commit);
        Box::pin(async move {
            let draft = tx
                .doc(
                    &NESTED_CALL_DOC,
                    DocOwner::TaskFamily(parent_task, key.as_str()),
                    None,
                )
                .await?;
            let mut state: NestedCallState = crate::harness::types::read_state(&draft)?;
            if let Some(existing) = state.task_id {
                let input = tx
                    .task(existing)
                    .await?
                    .and_then(|record| serde_json::from_value::<ToolTaskInput>(record.input).ok());
                let same = match &input {
                    Some(ToolTaskInput::Nested { call: prior, .. }) => {
                        prior.name == call.name
                            && crate::harness::json_equal(
                                &JsonValue::Object(prior.arguments.clone()),
                                &JsonValue::Object(call.arguments.clone()),
                            )
                    }
                    _ => false,
                };
                if !same {
                    return Err(SessionError::message(format!(
                        "Nested call {} was already made with another tool or other arguments",
                        call.id
                    )));
                }
                if let Ok(mut flag) = replayed.lock() {
                    *flag = state.result.is_some();
                }
                if let Ok(mut slot) = created.lock() {
                    *slot = Some(existing);
                }
                return Ok(None);
            }
            let input = ToolTaskInput::Nested {
                parent: parent_task,
                parent_call_id: parent_call_id.clone(),
                key: key.clone(),
                call: call.clone(),
                progress,
            };
            let task = tx
                .create_task(TaskCreate {
                    kind: TOOL_TASK_NAME.to_string(),
                    version: 2,
                    input: serde_json::to_value(&input).unwrap_or(JsonValue::Null),
                    checkpoint: serde_json::json!({ "phase": "call" }),
                    ownership: TaskOwnership::Task(parent_task),
                    conversation_id: Some(conversation_id),
                    background: false,
                    abandon_on_restart,
                    trace: Default::default(),
                    links: Vec::new(),
                })
                .await?;
            state.task_id = Some(task);
            crate::harness::types::write_state(&draft, &state)?;
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut live_state: LiveState = crate::harness::types::read_state(&live)?;
            let mut nested = live_state.nested_tools.take().unwrap_or_default();
            nested.push(NestedToolSlot {
                progress: crate::harness::live::SlotProgress {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    status: ToolSlotStatus::Pending,
                    ..Default::default()
                },
                parent_call_id: parent_call_id.clone(),
                parent_task_id: parent_task,
                task_id: task,
                arguments: call.arguments.clone(),
                summary: None,
            });
            live_state.nested_tools = Some(nested);
            crate::harness::types::write_state(&live, &live_state)?;
            if let Ok(mut slot) = created.lock() {
                *slot = Some(task);
            }
            Ok(None)
        })
    });
    runtime.commit(change, context).await?;
    let replayed = replayed.lock().map(|flag| *flag).unwrap_or(false);
    if replayed {
        context.telemetry().add_event(
            "pi.durable.replay",
            serde_json::json!({ "tool.name": name })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
    }
    let id = created.lock().ok().and_then(|slot| *slot);
    id.ok_or_else(|| SessionError::message("Nested call was not created"))
}

/// What the caller gets for a nested call without a stored result: one the
/// scheduler faulted or orphaned.
fn fallback_result(
    task_id: TaskId,
    name: &str,
    state: &TaskState,
) -> crate::harness::types::NestedToolExecutionResult {
    let outcome = match state {
        TaskState::Terminal { outcome } | TaskState::Completing { outcome } => outcome.clone(),
        _ => {
            return crate::harness::types::NestedToolExecutionResult {
                task_id,
                structured_output: None,
                is_error: true,
                diagnostics: vec![tool_diagnostic(
                    "pending",
                    &format!("Tool {name} ended without a result"),
                )],
                details: None,
                usage: None,
                duration_ms: None,
            };
        }
    };
    let (code, message) = match outcome {
        TaskOutcome::Faulted { error } => {
            ("faulted", format!("Tool {name} failed: {}", error.message))
        }
        TaskOutcome::Orphaned { reason } => (
            "orphaned",
            format!("Tool {name} could not resume: {reason}"),
        ),
        _ => ("ended", format!("Tool {name} ended without a result")),
    };
    let error = harness_error(code, &message);
    crate::harness::types::NestedToolExecutionResult {
        task_id,
        structured_output: None,
        is_error: true,
        details: None,
        diagnostics: error.diagnostics.unwrap_or_default(),
        usage: None,
        duration_ms: None,
    }
}

/// An error result from the slot's durable partial output, details, and
/// diagnostics.
fn from_slot(slot: Option<&SlotSnapshot>, code: &str, message: &str) -> ToolExecutionResult {
    let mut diagnostics = slot
        .and_then(|slot| slot.diagnostics.clone())
        .unwrap_or_default();
    let dropped_bytes = slot.and_then(|slot| slot.dropped_bytes).unwrap_or(0);
    if dropped_bytes > 0 {
        diagnostics.push(truncated(
            &crate::harness::output::BoundedOutput {
                text: String::new(),
                dropped_bytes,
                dropped_lines: slot.and_then(|slot| slot.dropped_lines).unwrap_or(0),
            },
            None,
        ));
    }
    diagnostics.push(tool_diagnostic(code, message));
    let output = slot
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
        output: Some(output),
        is_error: Some(true),
        details: slot.and_then(|slot| slot.details.clone()),
        diagnostics: Some(diagnostics),
        ..Default::default()
    }
}

/// Error text a nested call's summary keeps.
const MAX_SUMMARY_ERROR_CHARS: usize = 500;

/// How a nested call ended, for its slot: error text, from its diagnostics or
/// else its `output`, bounded.
fn summary_of(
    result: &crate::harness::types::NestedToolExecutionResult,
    output: &[ToolResultContent],
) -> NestedToolSummary {
    let error = if result.is_error {
        let text = error_text_of(result, output);
        text.chars().take(MAX_SUMMARY_ERROR_CHARS).collect()
    } else {
        String::new()
    };
    NestedToolSummary {
        is_error: result.is_error,
        duration_ms: result.duration_ms,
        usage: result.usage.clone(),
        error: if error.is_empty() { None } else { Some(error) },
    }
}

/// The error messages of a nested result, else the text items of its output,
/// joined with newlines.
fn error_text_of(
    result: &crate::harness::types::NestedToolExecutionResult,
    output: &[ToolResultContent],
) -> String {
    let errors: Vec<&str> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    if errors.is_empty() {
        output
            .iter()
            .filter_map(|item| match item {
                ToolResultContent::Text(text) => Some(text.text.as_str()),
                ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        errors.join("\n")
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
) -> SessionResult<crate::types::EntryRecord> {
    let diagnostics = result.diagnostics.clone().unwrap_or_default();
    let mut content = result.output.clone().unwrap_or_default();
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

/// The Harness's truncation diagnostic; `retain` is unknown when rebuilt from a
/// slot after recovery.
fn truncated(
    dropped: &crate::harness::output::BoundedOutput,
    retain: Option<OutputRetain>,
) -> ToolDiagnostic {
    let kept = match retain {
        Some(OutputRetain::Head) => " to its beginning",
        Some(OutputRetain::Tail) => " to its end",
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
/// the text items are replaced by one bounded item at the position of the first
/// (head) or last (tail) text item; other content is kept.
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
        OutputRetain::Head => content
            .iter()
            .position(|item| matches!(item, ToolResultContent::Text(_))),
        OutputRetain::Tail => content
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

/// Append a failed tool result entry for a tool the model was not offered.
pub fn harness_error(code: &str, message: &str) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Some(Vec::new()),
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

/// The called tool: the call as hooks see it, including a nested call's parent.
async fn read_call(
    runtime: &dyn TaskRuntime,
    input: &ToolTaskInput,
    context: &Context,
) -> SessionResult<ToolHookCall> {
    let call: ToolHookCall = match input {
        ToolTaskInput::Nested {
            parent,
            parent_call_id,
            call,
            ..
        } => Ok::<ToolHookCall, SessionError>(ToolHookCall {
            call: call.clone(),
            parent: Some(crate::harness::types::NestedCallParent {
                task_id: *parent,
                call_id: parent_call_id.clone(),
            }),
        }),
        ToolTaskInput::Model { assistant, call_id } => {
            let entry = runtime.entry(*assistant, context).await?.ok_or_else(|| {
                SessionError::message(format!("Entry {assistant} has no tool call {call_id}"))
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
                            AssistantContent::ToolCall(call) if &call.id == call_id => Some(call),
                            _ => None,
                        })
                });
            let call = call.ok_or_else(|| {
                SessionError::message(format!("Entry {assistant} has no tool call {call_id}"))
            })?;
            Ok::<ToolHookCall, SessionError>(ToolHookCall::model(call))
        }
    }?;
    context.telemetry().set_attributes(
        serde_json::json!({
            "tool.name": call.call.name,
            "tool.call_id": call.call.id,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    Ok(call)
}

async fn read_slot(
    runtime: &dyn TaskRuntime,
    context: &Context,
) -> SessionResult<Option<SlotSnapshot>> {
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
    let Some(state) = state else {
        return Ok(None);
    };
    if let Some(slot) = state
        .tools
        .as_ref()
        .and_then(|tools| tools.iter().find(|slot| slot.task_id == Some(task_id)))
    {
        return Ok(Some(SlotSnapshot::from(&slot.progress)));
    }
    Ok(state
        .nested_tools
        .as_ref()
        .and_then(|nested| nested.iter().find(|slot| slot.task_id == task_id))
        .map(|slot| SlotSnapshot::from(&slot.progress)))
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
            if let Some(mut slot) = tool_slot_mut(&mut state, task_id) {
                clear_progress(slot.progress());
            }
            crate::harness::types::write_state(&live, &state)?;
            Ok(None)
        })
    });
    runtime.commit(change, context).await
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

/// Explicit nested call keys are path segments of the call ID, so IDs of
/// different nested calls never collide, and are never plain positive integers,
/// which default keys use.
fn check_key(key: &str) -> Result<(), String> {
    let positive_integer = !key.is_empty()
        && !key.starts_with('0')
        && key.chars().all(|character| character.is_ascii_digit());
    if key.is_empty() || key.contains('/') || key == "__proto__" || positive_integer {
        return Err(format!(
            "Nested call key {key:?} must be non-empty, without \"/\", not \"__proto__\", and not a positive integer"
        ));
    }
    Ok(())
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
    admissions: Arc<AdmissionTracker>,
    sequence: Arc<AtomicUsize>,
    resumes: bool,
}

impl ToolApi {
    #[allow(clippy::too_many_arguments)]
    fn new(
        runtime: &dyn TaskRuntime,
        call_id: String,
        reported: Arc<Mutex<Reported>>,
        env: Option<Arc<dyn crate::env::types::ExecutionEnv>>,
        progress: crate::harness::output::Progress,
        admissions: Arc<AdmissionTracker>,
        sequence: Arc<AtomicUsize>,
        resumes: bool,
    ) -> Self {
        Self {
            runtime: runtime.handle(),
            call_id,
            env,
            reported,
            progress,
            admissions,
            sequence,
            resumes,
        }
    }

    fn assert_live(&self) -> Result<(), String> {
        if self.admissions.is_ended() {
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

    fn models(&self) -> Models {
        self.runtime.models()
    }

    fn env(&self) -> Option<Arc<dyn crate::env::types::ExecutionEnv>> {
        self.env.clone()
    }

    fn now(&self) -> Result<f64, SessionError> {
        self.runtime.now()
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
        if reported.limits.retain != OutputRetain::Tail {
            return None;
        }
        Some(crate::harness::types::OutputWindow {
            max_bytes: reported.limits.max_bytes,
            max_lines: reported.limits.max_lines,
            min_interval_ms: self.runtime.settings().progress.output_interval_ms,
            bytes_per_second: crate::harness::output::PROGRESS_BYTES_PER_SECOND,
        })
    }

    fn retained_output(&self) -> Result<crate::harness::types::RetainedOutput, String> {
        self.assert_live()?;
        let mut reported = self.reported.lock().map_err(|_| "tool state".to_string())?;
        let retained = reported.output.snapshot();
        Ok(crate::harness::types::RetainedOutput {
            text: retained.text,
            truncated: retained.dropped_bytes > 0,
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

    async fn create_task(
        &self,
        definition: crate::harness::types::AnyTask,
        input: JsonValue,
        ownership: TaskOwnership,
        background: bool,
        abandon_on_restart: Option<bool>,
        context: &Context,
    ) -> Result<TaskId, SessionError> {
        self.assert_live().map_err(SessionError::message)?;
        let abandon_on_restart = abandon_on_restart
            .unwrap_or(matches!(ownership, TaskOwnership::Task(_)) && !self.resumes);
        let conversation_id = self.runtime.conversation_id();
        let kind = definition.name().to_string();
        let version = definition.version();
        let checkpoint = definition.initial();
        let change: ToolCommitFn = Arc::new(move |tx| {
            let kind = kind.clone();
            let input = input.clone();
            let checkpoint = checkpoint.clone();
            Box::pin(async move {
                let task = tx
                    .create_task(TaskCreate {
                        kind,
                        version,
                        input,
                        checkpoint,
                        ownership,
                        conversation_id: Some(conversation_id),
                        background,
                        abandon_on_restart,
                        trace: Default::default(),
                        links: Vec::new(),
                    })
                    .await?;
                Ok(JsonValue::from(task.get()))
            })
        });
        let created = self.commit(change, context).await?;
        created
            .as_u64()
            .map(TaskId::from_number)
            .ok_or_else(|| SessionError::message("Created task ID is unavailable"))
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

    async fn execute_tool(
        &self,
        name: &str,
        args: JsonObject,
        context: &Context,
        options: crate::harness::types::NestedCallOptions,
    ) -> Result<crate::harness::types::NestedToolExecutionResult, SessionError> {
        self.assert_live().map_err(SessionError::message)?;
        let key = match &options.key {
            Some(key) => {
                check_key(key).map_err(SessionError::message)?;
                key.clone()
            }
            None => {
                let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
                sequence.to_string()
            }
        };
        context.telemetry().add_link(serde_json::json!({
            "task.id": self.runtime.task_id().get(),
            "tool.call_id": self.call_id,
        }));
        // Registered before the memo, so cleanup waits for the admission.
        let admission = self.admissions.begin().map_err(SessionError::message)?;
        let started = start_nested_call(
            self.runtime.as_ref(),
            &self.call_id,
            name,
            &args,
            &key,
            options.progress,
            !self.resumes,
            context,
        )
        .await;
        drop(admission);
        let id = started?;
        let _ = self
            .runtime
            .memo_set(MADE_NESTED_CALLS, JsonValue::Bool(true), context)
            .await?;
        let settled = self.runtime.wait_for_task(id, context).await?;
        let stored = self
            .runtime
            .snapshot(
                &NESTED_CALL_DOC,
                DocOwner::TaskFamily(self.runtime.task_id(), key.as_str()),
                context,
            )
            .await?
            .and_then(|value| value.get("result").cloned())
            .and_then(|value| {
                serde_json::from_value::<crate::harness::types::NestedToolExecutionResult>(value)
                    .ok()
            });
        Ok(stored.unwrap_or_else(|| fallback_result(id, name, &settled.record.state)))
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
        assert_eq!(result.output.as_ref().map(Vec::len), Some(0));
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

    #[test]
    fn nested_keys_reject_path_segments_and_default_keys() {
        assert!(check_key("my-key").is_ok());
        assert!(check_key("").is_err());
        assert!(check_key("a/b").is_err());
        assert!(check_key("1").is_err());
        assert!(check_key("0").is_ok());
        assert!(check_key("__proto__").is_err());
    }
}
