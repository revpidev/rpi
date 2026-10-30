//! Built-in generation task (port of
//! `packages/durable/src/harness/generation.ts` @ `4a080252f`).
//!
//! The generation task prepares the positional system prompt and tool loadout,
//! requests or classifies the model response, retries retryable failures, and
//! runs tool rounds. The run's inputs live in `pi.live.run`; the task that owns
//! `run.taskId` settles them.
//!
//! Ported phases: `prepare`, `request`, `retry`, `poll`, `tools`.
//!
//! Rust shape notes: upstream `streamResponse` throttles partials with a timer
//! and one commit in flight; the port commits synchronously when
//! `progress.partialIntervalMs` elapsed since the last flush. `author` hooks
//! take an erased [`RuntimeHookApi`]. `renderSections`' error path collapses to
//! an infallible renderer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use rpi_ai::models::{
    ModelsDeferredCancelOptions, ModelsDeferredFetchOptions, ModelsSimpleStreamOptions,
};
use rpi_ai::types::{
    AssistantContent, AssistantMessage, Context as ModelContext, DeferredFetchOptions, Message,
    ModelThinkingLevel, ProviderRequestOptions, SimpleStreamOptions, StopReason, StreamEvent, Tool,
    ToolCall, UserMessage, UserRole,
};
use rpi_ai::utils::overflow::is_context_overflow;
use rpi_ai::utils::retry::{is_retryable_assistant_error, retry_delay_ms};
use rpi_ai::utils::transcript::get_current_tools;
use serde::{Deserialize, Serialize};

use crate::chord::context::Context;
use crate::chord::json::JsonValue;
use crate::entries::{ASSISTANT_ENTRY, RESET_ENTRY, USER_ENTRY};
use crate::harness::agent::{add_tools, agent_hooks};
use crate::harness::compaction::{
    create_compaction, estimate_context, reasoning_level, request_stream_options, select_cut,
};
use crate::harness::inbox::{
    BoundaryAt, INBOX_DOC, InboxItem, InboxState, QueueModes, apply_boundary, prepare_boundary,
};
use crate::harness::json::assign_json;
use crate::harness::live::{
    LIVE_DOC, LiveState, SlotProgress, ToolSlot, ToolSlotStatus, convert_partial, end_run,
};
use crate::harness::prompt::{plan_system_entries, render_sections, replay_sections};
use crate::harness::registry::{GENERATION_TASK_NAME, TOOL_TASK_NAME};
use crate::harness::tool::{append_tool_result, harness_error};
use crate::harness::types::{
    AfterFailedRun, ConversationStreamOptions, HookHandlers, ModelRef, NextTaskState, PromptInput,
    RunningTask, TaskCommitFn, TaskDefinition, TaskRuntime, TimerDisarm, ToolControl,
    ToolExecutionMode, read_state, write_state,
};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::ids::{ConversationId, EntryId, SubmissionId, TaskId};
use crate::session::{
    DocDraft, DocOwner, EntryDraft, EntryHead, SessionError, SessionResult, SubmissionSettlement,
    TaskCreate, TaskOwnership, Transaction,
};
use crate::types::{EntryQuery, JoinPolicy, TaskOutcome, TaskOutcomeError};

/// Generation task input (none).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationInput {}

/// Durable generation checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
enum GenerationCheckpoint {
    /// Rendering the prompt and tool loadout.
    #[serde(rename = "prepare")]
    Prepare {
        /// Attempt number.
        attempt: u64,
        /// The blocking compaction this generation waited for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Overflow text checked once when `prepare` resumes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overflow: Option<String>,
        /// Set when the blocking threshold started the compaction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<bool>,
    },
    /// The pinned request.
    #[serde(rename = "request")]
    Request {
        /// Attempt number.
        attempt: u64,
        /// The blocking compaction this generation waited for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Resolved model.
        model: ModelRef,
        /// Thinking level.
        thinking_level: ModelThinkingLevel,
        /// Pinned request options.
        stream_options: ConversationStreamOptions,
        /// Newest entry included in the request.
        cutoff: EntryId,
    },
    /// Retry backoff.
    #[serde(rename = "retry")]
    Retry {
        /// Attempt number.
        attempt: u64,
        /// The blocking compaction this generation waited for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Wall-clock time of the next attempt.
        until: f64,
    },
    /// Polling a provider-side deferred response.
    #[serde(rename = "poll")]
    Poll {
        /// Attempt number.
        attempt: u64,
        /// The blocking compaction this generation waited for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Resolved model.
        model: ModelRef,
        /// Newest entry included in the request.
        cutoff: EntryId,
        /// Provider handle persisted across restarts.
        handle: rpi_ai::types::DeferredHandle,
        /// Harness-clock time of the next poll.
        #[serde(rename = "pollAt")]
        poll_at: f64,
    },
    /// Waiting on the round's tool tasks.
    #[serde(rename = "tools")]
    Tools {
        /// The tool-calling answer.
        assistant: EntryId,
        /// Tool tasks created so far, in call order.
        tools: Vec<TaskId>,
        /// Calls of a sequential round not started yet, in call order.
        pending: Vec<String>,
    },
}

/// What classification needs from the request that produced a message.
struct Request {
    attempt: u64,
    compacted: Option<TaskId>,
    model: ModelRef,
    cutoff: EntryId,
    /// Previous poll time, used to keep repeated polls strictly increasing.
    poll_at: Option<f64>,
    /// Committed model context through `cutoff`.
    messages: Vec<Message>,
}

/// Which threshold compaction preparation starts before its request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThresholdKind {
    Blocking,
    Background,
}

struct DisarmOnDrop(TimerDisarm);

impl Drop for DisarmOnDrop {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Built-in generation task (`pi.generation`).
pub struct GenerationTask;

#[async_trait]
impl TaskDefinition for GenerationTask {
    fn name(&self) -> &str {
        GENERATION_TASK_NAME
    }

    fn version(&self) -> u64 {
        1
    }

    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "prepare", "attempt": 1 })
    }

    async fn run_phase(
        &self,
        phase: &str,
        task: RunningTask,
        runtime: &dyn TaskRuntime,
        context: &Context,
    ) -> SessionResult<()> {
        let checkpoint = parse_checkpoint(task.checkpoint());
        match phase {
            "prepare" => prepare(&task, runtime, context, checkpoint).await,
            "request" => request(runtime, context, checkpoint).await,
            "retry" => retry(runtime, context, checkpoint).await,
            "poll" => poll(runtime, context, checkpoint).await,
            "tools" => tools(runtime, context, checkpoint).await,
            _ => Ok(()),
        }
    }

    async fn abort(
        &self,
        task: RunningTask,
        runtime: &dyn TaskRuntime,
        context: &Context,
    ) -> SessionResult<()> {
        let checkpoint = parse_checkpoint(task.checkpoint());
        if let GenerationCheckpoint::Poll {
            model: reference,
            handle,
            ..
        } = &checkpoint
            && let Some(model) = runtime
                .models()
                .get_model(&reference.provider, &reference.model_id)
        {
            let options = ModelsDeferredCancelOptions::from(rpi_ai::types::StreamOptions {
                request: ProviderRequestOptions {
                    signal: Some(runtime.signal()),
                    ..Default::default()
                },
                ..Default::default()
            });
            if let Err(error) = runtime
                .models()
                .cancel_deferred(&model, handle, Some(options))
                .await
            {
                let _ = runtime.report(error.message);
            }
        }
        let unstarted = match &checkpoint {
            GenerationCheckpoint::Tools {
                assistant, pending, ..
            } => read_calls(runtime, *assistant, pending, context).await?,
            _ => Vec::new(),
        };
        let conversation_id = runtime.conversation_id();
        let task_id = runtime.task_id();
        let now = runtime.now()? as i64;
        let change: TaskCommitFn = Arc::new(move |tx, _current| {
            let unstarted = unstarted.clone();
            Box::pin(async move {
                let live = tx
                    .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                    .await?;
                convert_partial(tx, &live).await?;
                for call in &unstarted {
                    let result =
                        harness_error("aborted", &format!("Tool {} was aborted", call.name));
                    append_tool_result(tx, conversation_id, call, &result, now, None).await?;
                }
                end_run(
                    tx,
                    &live,
                    task_id,
                    SubmissionSettlement::Unanswered {
                        reason: "aborted".to_string(),
                        detail: None,
                    },
                )
                .await?;
                Ok(Some(NextTaskState::Terminal {
                    outcome: TaskOutcome::Aborted {
                        reason: Some("aborted".to_string()),
                        result: None,
                    },
                }))
            })
        });
        runtime.commit(change, context).await
    }
}

fn parse_checkpoint(checkpoint: Option<&JsonValue>) -> GenerationCheckpoint {
    match checkpoint {
        Some(value) => {
            serde_json::from_value(value.clone()).unwrap_or(GenerationCheckpoint::Prepare {
                attempt: 1,
                compacted: None,
                overflow: None,
                threshold: None,
            })
        }
        None => GenerationCheckpoint::Prepare {
            attempt: 1,
            compacted: None,
            overflow: None,
            threshold: None,
        },
    }
}

/// The tool calls `call_ids` of the assistant entry, in the given order.
async fn read_calls(
    runtime: &dyn TaskRuntime,
    assistant: EntryId,
    call_ids: &[String],
    context: &Context,
) -> SessionResult<Vec<ToolCall>> {
    let entry = runtime.entry(assistant, context).await?;
    let calls: Vec<ToolCall> = entry
        .and_then(|record| record.model)
        .and_then(|messages| {
            messages.into_iter().find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
        })
        .map(|assistant| {
            assistant
                .content
                .into_iter()
                .filter_map(|content| match content {
                    AssistantContent::ToolCall(call) => Some(call),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(call_ids
        .iter()
        .filter_map(|id| calls.iter().find(|call| call.id == *id).cloned())
        .collect())
}

// ---------------------------------------------------------------------------
// prepare
// ---------------------------------------------------------------------------

async fn prepare(
    _task: &RunningTask,
    runtime: &dyn TaskRuntime,
    context: &Context,
    checkpoint: GenerationCheckpoint,
) -> SessionResult<()> {
    let (attempt, compacted, overflow, threshold) = match checkpoint {
        GenerationCheckpoint::Prepare {
            attempt,
            compacted,
            overflow,
            threshold,
        } => (attempt, compacted, overflow, threshold),
        _ => (1, None, None, None),
    };
    let agent = runtime.agent(context).await?;
    let settings = runtime.settings();
    let Some(reference) = agent.model.clone() else {
        return fail_no_model(runtime, None, context).await;
    };
    let Some(model) = runtime
        .models()
        .get_model(&reference.provider, &reference.model_id)
    else {
        return fail_no_model(runtime, Some(&reference), context).await;
    };
    if let (Some(compacted), Some(overflow)) = (compacted, overflow.clone()) {
        let outcomes = runtime.outcomes(vec![compacted], context).await?;
        let completed = matches!(
            outcomes.first(),
            Some(TaskOutcome::Completed { result }) if result.get("entryId").is_some()
        );
        if !completed {
            return fail_model_error(runtime, overflow, context).await;
        }
    }
    // A threshold compaction that found nothing to compact or was declined
    // leaves a request that may still fit. One that failed leaves a context it
    // judged too large, so the run fails and keeps the failure visible.
    if let (Some(compacted), Some(true)) = (compacted, threshold) {
        let outcomes = runtime.outcomes(vec![compacted], context).await?;
        if let Some(outcome) = outcomes.first()
            && !matches!(outcome, TaskOutcome::Completed { .. })
        {
            return fail_model_error(runtime, compaction_failure(outcome), context).await;
        }
    }
    let conversation_id = runtime.conversation_id();
    let view = runtime.context(conversation_id, context, None).await?;
    let shown = replay_sections(&view.messages);
    let shown: BTreeMap<String, String> = shown
        .into_iter()
        .filter_map(|(key, value)| value.as_str().map(|text| (key, text.to_string())))
        .collect();
    let env = match runtime.env(context).await {
        Ok(env) => env,
        Err(error) => {
            if runtime.signal().is_cancelled() {
                return Err(error);
            }
            let _ = runtime.report(error.to_string());
            runtime.report_info(
                crate::harness::types::ReportInfo::new(
                    crate::harness::types::ReportKind::HostError,
                )
                .with_task(runtime.task_id(), GENERATION_TASK_NAME)
                .with_reason("env"),
            );
            None
        }
    };
    let input = PromptInput {
        conversation_id,
        agent: agent.clone(),
        env,
        shown,
        read: runtime.document_reader(),
    };
    let desired = render_sections(&agent.sections, input, context).await;
    let tools: Vec<Tool> = agent.tools.iter().map(|tool| tool.tool.clone()).collect();
    let timestamp = runtime.now()? as i64;
    let entries = plan_system_entries(&view, &desired, &tools, timestamp);
    let threshold = if compacted.is_none() {
        threshold_compaction(
            &view,
            &entries,
            model.context_window as u64,
            crate::harness::agent::compaction_for(&settings, &reference),
        )
    } else {
        None
    };
    let task_id = runtime.task_id();
    if threshold == Some(ThresholdKind::Blocking) {
        let change: TaskCommitFn = Arc::new(move |tx, _current| {
            Box::pin(async move {
                let child = create_compaction(
                    tx,
                    conversation_id,
                    crate::harness::types::CompactionReason::Threshold,
                    None,
                    Some(task_id),
                )
                .await?;
                Ok(Some(NextTaskState::Waiting {
                    checkpoint: to_checkpoint(GenerationCheckpoint::Prepare {
                        attempt,
                        compacted: Some(child),
                        overflow: None,
                        threshold: Some(true),
                    }),
                    on: vec![child],
                    policy: JoinPolicy::AllSettled,
                }))
            })
        });
        return runtime.commit(change, context).await;
    }
    let stream_options = settings.stream.clone();
    let thinking_level = agent.thinking_level;
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let entries = entries.clone();
        let reference = reference.clone();
        let stream_options = stream_options.clone();
        Box::pin(async move {
            let mut cutoff: Option<EntryId> = tx
                .scan_entries(
                    EntryQuery {
                        conversation_id,
                        min_entry_id: None,
                        max_entry_id: None,
                        order: None,
                    },
                    1,
                    None,
                )
                .await?
                .items
                .first()
                .map(|entry| entry.id);
            for entry in &entries {
                let record = tx.append_entry(conversation_id, entry.clone()).await?;
                cutoff = Some(record.id);
            }
            let Some(cutoff) = cutoff else {
                return Err(SessionError::message(format!(
                    "Conversation {conversation_id} has no entries to send"
                )));
            };
            if threshold == Some(ThresholdKind::Background) {
                let live = tx
                    .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                    .await?;
                let state: LiveState = crate::harness::types::read_state(&live)?;
                if state.compactions.is_none() {
                    create_compaction(
                        tx,
                        conversation_id,
                        crate::harness::types::CompactionReason::Threshold,
                        None,
                        None,
                    )
                    .await?;
                }
            }
            Ok(Some(NextTaskState::Running {
                checkpoint: to_checkpoint(GenerationCheckpoint::Request {
                    attempt,
                    compacted,
                    model: reference,
                    thinking_level,
                    stream_options,
                    cutoff,
                }),
            }))
        })
    });
    runtime.commit(change, context).await
}

/// Which threshold compaction preparation starts before its request, if any.
fn threshold_compaction(
    view: &crate::harness::types::ContextView,
    planned: &[EntryDraft],
    context_window: u64,
    policy: crate::harness::types::CompactionPolicy,
) -> Option<ThresholdKind> {
    if !policy.enabled || context_window == 0 {
        return None;
    }
    let extra: Vec<Message> = planned
        .iter()
        .flat_map(|entry| entry.model.clone().unwrap_or_default())
        .collect();
    let tokens = estimate_context(view, &extra);
    let blocking = context_window.saturating_sub(policy.reserve_tokens);
    let background = blocking.saturating_sub(policy.background_tokens);
    let over = if tokens > blocking {
        Some(ThresholdKind::Blocking)
    } else if policy.background_tokens > 0 && tokens > background {
        Some(ThresholdKind::Background)
    } else {
        None
    }?;
    select_cut(view, policy.keep_recent_tokens)?;
    Some(over)
}

// ---------------------------------------------------------------------------
// request
// ---------------------------------------------------------------------------

async fn request(
    runtime: &dyn TaskRuntime,
    context: &Context,
    checkpoint: GenerationCheckpoint,
) -> SessionResult<()> {
    let (attempt, compacted, reference, thinking_level, stream_options, cutoff) = match checkpoint {
        GenerationCheckpoint::Request {
            attempt,
            compacted,
            model,
            thinking_level,
            stream_options,
            cutoff,
        } => (
            attempt,
            compacted,
            model,
            thinking_level,
            stream_options,
            cutoff,
        ),
        _ => return Ok(()),
    };
    let conversation_id = runtime.conversation_id();
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            convert_partial(tx, &live).await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            state.generation = Some(crate::harness::live::LiveGeneration {
                attempt,
                ..Default::default()
            });
            crate::harness::types::write_state(&live, &state)?;
            Ok(None)
        })
    });
    runtime.commit(change, context).await?;
    let Some(model) = runtime
        .models()
        .get_model(&reference.provider, &reference.model_id)
    else {
        return fail_no_model(runtime, Some(&reference), context).await;
    };
    let view = runtime
        .context(conversation_id, context, Some(cutoff))
        .await?;
    let mut messages = view.messages.clone();
    let agent = runtime.agent(context).await?;
    for handlers in agent_hooks(&agent, GENERATION_TASK_NAME) {
        if let HookHandlers::Generation(hooks) = handlers {
            let api = crate::harness::types::RuntimeHookApi { runtime };
            if let Some(replaced) = hooks
                .before_request(
                    crate::harness::types::RequestMessages {
                        messages: messages.clone(),
                    },
                    &api,
                    context,
                )
                .await
            {
                messages = replaced.messages;
            }
        }
    }
    let session_id = runtime.provider_session_id(context).await?;
    let stream = request_stream_options(&stream_options, runtime.signal(), session_id);
    let simple = SimpleStreamOptions {
        stream,
        reasoning: reasoning_level(thinking_level),
        thinking_budgets: None,
        tool_choice: None,
    };
    let model_context = ModelContext {
        system_prompt: None,
        messages,
        tools: None,
    };
    let message = stream_response(runtime, &model, model_context, simple, attempt, context).await?;
    let request = Request {
        attempt,
        compacted,
        model: reference,
        cutoff,
        poll_at: None,
        messages: view.messages,
    };
    classify(runtime, request, message, context).await
}

const DEFAULT_POLL_AFTER_MS: u64 = 5_000;

async fn poll(
    runtime: &dyn TaskRuntime,
    context: &Context,
    checkpoint: GenerationCheckpoint,
) -> SessionResult<()> {
    let GenerationCheckpoint::Poll {
        attempt,
        compacted,
        model: reference,
        cutoff,
        handle,
        poll_at,
    } = checkpoint
    else {
        return Ok(());
    };
    if poll_at > runtime.now()? {
        let checkpoint = GenerationCheckpoint::Poll {
            attempt,
            compacted,
            model: reference,
            cutoff,
            handle,
            poll_at,
        };
        return runtime
            .sleep_until(
                poll_at,
                serde_json::to_value(checkpoint).unwrap_or(JsonValue::Null),
                context,
            )
            .await;
    }
    let Some(model) = runtime
        .models()
        .get_model(&reference.provider, &reference.model_id)
    else {
        return fail_no_model(runtime, Some(&reference), context).await;
    };
    let options = ModelsDeferredFetchOptions::from(DeferredFetchOptions {
        stream: rpi_ai::types::StreamOptions {
            request: ProviderRequestOptions {
                signal: Some(runtime.signal()),
                ..Default::default()
            },
            ..Default::default()
        },
        wait: None,
    });
    let message = runtime
        .models()
        .fetch_deferred(&model, &handle, Some(options))
        .await
        .ok_or_else(|| SessionError::message("Deferred fetch ended without a result"))?;
    let view = runtime
        .context(runtime.conversation_id(), context, Some(cutoff))
        .await?;
    let request = Request {
        attempt,
        compacted,
        model: reference,
        cutoff,
        poll_at: Some(poll_at),
        messages: view.messages,
    };
    classify(runtime, request, message, context).await
}

/// Append a committed partial as it streams, at most once per
/// `progress.partialIntervalMs`.
async fn stream_response(
    runtime: &dyn TaskRuntime,
    model: &rpi_ai::types::Model,
    model_context: ModelContext,
    options: SimpleStreamOptions,
    attempt: u64,
    context: &Context,
) -> SessionResult<AssistantMessage> {
    let interval = runtime.settings().progress.partial_interval_ms as f64;
    let mut events = runtime.models().stream_simple(
        model,
        &model_context,
        Some(ModelsSimpleStreamOptions::from(options)),
    );
    let token = runtime.signal();
    let mut pending: Option<Arc<AssistantMessage>> = None;
    loop {
        if token.is_cancelled() {
            return Err(SessionError::Aborted);
        }
        let fired = Arc::new(tokio::sync::Notify::new());
        let callback = Arc::clone(&fired);
        let _disarm = DisarmOnDrop(runtime.set_timer(
            Arc::new(move || callback.notify_one()),
            interval.max(0.0) as u64,
        )?);
        let next = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(SessionError::Aborted),
            _ = fired.notified() => {
                if let Some(partial) = pending.take() {
                    commit_partial(runtime, &partial, attempt, context).await?;
                }
                continue;
            }
            event = events.next() => event,
        };
        let Some(event) = next else {
            break;
        };
        let partial = match &event {
            StreamEvent::Done { .. } | StreamEvent::Error { .. } => break,
            event => partial_of(event),
        };
        if let Some(partial) = partial
            && !partial.content.is_empty()
        {
            pending = Some(partial);
        }
    }
    events
        .result()
        .await
        .ok_or_else(|| SessionError::message("Model stream ended without a result"))
}

fn partial_of(event: &StreamEvent) -> Option<Arc<AssistantMessage>> {
    match event {
        StreamEvent::Start { partial }
        | StreamEvent::TextStart { partial, .. }
        | StreamEvent::TextDelta { partial, .. }
        | StreamEvent::TextEnd { partial, .. }
        | StreamEvent::ThinkingStart { partial, .. }
        | StreamEvent::ThinkingDelta { partial, .. }
        | StreamEvent::ThinkingEnd { partial, .. }
        | StreamEvent::ToolCallStart { partial, .. }
        | StreamEvent::ToolCallDelta { partial, .. }
        | StreamEvent::ToolCallEnd { partial, .. } => Some(Arc::clone(partial)),
        StreamEvent::Done { .. } | StreamEvent::Error { .. } => None,
    }
}

async fn commit_partial(
    runtime: &dyn TaskRuntime,
    partial: &AssistantMessage,
    attempt: u64,
    context: &Context,
) -> SessionResult<()> {
    let conversation_id = runtime.conversation_id();
    let message = serde_json::to_value(partial).unwrap_or(JsonValue::Null);
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let message = message.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut doc = live.read()?;
            if doc.get("generation").is_none() {
                doc["generation"] = serde_json::json!({ "attempt": attempt });
            }
            if let Some(generation) = doc.get_mut("generation") {
                assign_json(generation, "message", message);
            }
            let mut guard = live.write()?;
            *guard = doc;
            Ok(None)
        })
    });
    runtime.commit(change, context).await
}

// ---------------------------------------------------------------------------
// retry
// ---------------------------------------------------------------------------

async fn retry(
    runtime: &dyn TaskRuntime,
    context: &Context,
    checkpoint: GenerationCheckpoint,
) -> SessionResult<()> {
    let (attempt, compacted, until) = match checkpoint {
        GenerationCheckpoint::Retry {
            attempt,
            compacted,
            until,
        } => (attempt, compacted, until),
        _ => return Ok(()),
    };
    if until > runtime.now()? {
        let checkpoint = to_checkpoint(GenerationCheckpoint::Retry {
            attempt,
            compacted,
            until,
        });
        runtime.sleep_until(until, checkpoint, context).await?;
        return Ok(());
    }
    let conversation_id = runtime.conversation_id();
    let next = attempt + 1;
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            state.generation = Some(crate::harness::live::LiveGeneration {
                attempt: next,
                ..Default::default()
            });
            crate::harness::types::write_state(&live, &state)?;
            Ok(Some(NextTaskState::Running {
                checkpoint: to_checkpoint(GenerationCheckpoint::Prepare {
                    attempt: next,
                    compacted,
                    overflow: None,
                    threshold: None,
                }),
            }))
        })
    });
    runtime.commit(change, context).await
}

// ---------------------------------------------------------------------------
// classify
// ---------------------------------------------------------------------------

async fn classify(
    runtime: &dyn TaskRuntime,
    request: Request,
    message: AssistantMessage,
    context: &Context,
) -> SessionResult<()> {
    if runtime.signal().is_cancelled() {
        return Err(SessionError::Aborted);
    }
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let Request {
        attempt,
        compacted,
        model: reference,
        cutoff,
        poll_at: previous_poll_at,
        messages,
    } = request;
    if message.stop_reason == StopReason::Deferred
        && let Some(handle) = message.deferred.clone()
    {
        let requested =
            runtime.now()? + handle.poll_after_ms.unwrap_or(DEFAULT_POLL_AFTER_MS) as f64;
        let poll_at = requested.max(
            previous_poll_at
                .map(|previous| previous + 1.0)
                .unwrap_or(f64::NEG_INFINITY),
        );
        let checkpoint = GenerationCheckpoint::Poll {
            attempt,
            compacted,
            model: reference,
            cutoff,
            handle: *handle,
            poll_at,
        };
        let change: TaskCommitFn = Arc::new(move |_tx, _current| {
            let checkpoint = checkpoint.clone();
            Box::pin(async move {
                let live = _tx
                    .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                    .await?;
                let mut state: LiveState = read_state(&live)?;
                state.generation = Some(crate::harness::live::LiveGeneration {
                    attempt,
                    message: None,
                    retry: None,
                    deferred: Some(crate::harness::live::DeferredStatus { poll_at }),
                });
                write_state(&live, &state)?;
                Ok(Some(NextTaskState::Sleeping {
                    checkpoint: serde_json::to_value(checkpoint).unwrap_or(JsonValue::Null),
                    until: poll_at,
                }))
            })
        });
        return runtime.commit(change, context).await;
    }
    if message.stop_reason == StopReason::Deferred {
        return fail_model_error(
            runtime,
            "Deferred model response has no handle".to_string(),
            context,
        )
        .await;
    }
    let agent = runtime.agent(context).await?;
    for handlers in agent_hooks(&agent, GENERATION_TASK_NAME) {
        if let HookHandlers::Generation(hooks) = handlers {
            let api = crate::harness::types::RuntimeHookApi { runtime };
            hooks.after_response(&message, &api, context).await;
        }
    }
    let calls: Vec<ToolCall> = message
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    if message.stop_reason == StopReason::ToolUse && !calls.is_empty() {
        return start_tool_round(
            runtime, &reference, cutoff, &messages, &message, &calls, context,
        )
        .await;
    }
    if matches!(
        message.stop_reason,
        StopReason::Stop | StopReason::Length | StopReason::ToolUse
    ) {
        return answer(runtime, &message, context).await;
    }
    let settings = runtime.settings();
    let overflow = message.stop_reason == StopReason::Error && is_context_overflow(&message, None);
    let compaction = crate::harness::agent::compaction_for(&settings, &reference);
    if overflow && compacted.is_none() && compaction.enabled {
        let view = runtime
            .context(conversation_id, context, Some(cutoff))
            .await?;
        if select_cut(&view, compaction.keep_recent_tokens).is_some() {
            let text = message
                .error_message
                .clone()
                .unwrap_or_else(|| "Context overflow".to_string());
            let change: TaskCommitFn = Arc::new(move |tx, _current| {
                let message = message.clone();
                let text = text.clone();
                Box::pin(async move {
                    let live = tx
                        .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                        .await?;
                    append_assistant(tx, conversation_id, &message).await?;
                    let mut state: LiveState = crate::harness::types::read_state(&live)?;
                    state.generation = None;
                    crate::harness::types::write_state(&live, &state)?;
                    let child = create_compaction(
                        tx,
                        conversation_id,
                        crate::harness::types::CompactionReason::Overflow,
                        None,
                        Some(task_id),
                    )
                    .await?;
                    Ok(Some(NextTaskState::Waiting {
                        checkpoint: to_checkpoint(GenerationCheckpoint::Prepare {
                            attempt,
                            compacted: Some(child),
                            overflow: Some(text),
                            threshold: None,
                        }),
                        on: vec![child],
                        policy: JoinPolicy::AllSettled,
                    }))
                })
            });
            return runtime.commit(change, context).await;
        }
    }
    let policy = settings.retry;
    let after = settings.after_failed_run;
    let modes = QueueModes {
        steering_mode: settings.steering_mode,
        follow_up_mode: settings.follow_up_mode,
    };
    let now = runtime.now()?;
    // An overflow is never retried: only a compaction can make the next request fit.
    let retry = message.stop_reason == StopReason::Error
        && !overflow
        && is_retryable_assistant_error(&message)
        && policy.enabled
        && attempt <= policy.max_retries as u64;
    let until = if retry {
        runtime.now()?
            + retry_delay_ms(
                policy.base_delay_ms,
                policy.max_agent_delay_ms,
                attempt as u32,
            ) as f64
    } else {
        0.0
    };
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let message = message.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let prepared = if !retry && after == AfterFailedRun::Continue {
                Some(prepare_boundary(tx, conversation_id, modes).await?)
            } else {
                None
            };
            append_assistant(tx, conversation_id, &message).await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            if retry {
                state.generation = Some(crate::harness::live::LiveGeneration {
                    attempt,
                    retry: Some(crate::harness::live::RetryStatus {
                        at: until,
                        error: message.error_message.clone().unwrap_or_default(),
                    }),
                    ..Default::default()
                });
                crate::harness::types::write_state(&live, &state)?;
                return Ok(Some(NextTaskState::Running {
                    checkpoint: to_checkpoint(GenerationCheckpoint::Retry {
                        attempt,
                        compacted,
                        until,
                    }),
                }));
            }
            let text = message.error_message.clone().unwrap_or_else(|| {
                format!(
                    "Model response ended with stop reason {:?}",
                    message.stop_reason
                )
            });
            end_run(
                tx,
                &live,
                task_id,
                SubmissionSettlement::Unanswered {
                    reason: "model_error".to_string(),
                    detail: Some(JsonValue::String(text.clone())),
                },
            )
            .await?;
            apply_after_failed_run(tx, conversation_id, &live, after, modes, now, prepared).await?;
            Ok(Some(NextTaskState::Terminal {
                outcome: TaskOutcome::Failed {
                    error: TaskOutcomeError {
                        message: text,
                        detail: Some(serde_json::json!({ "reason": "model_error" })),
                    },
                    result: None,
                },
            }))
        })
    });
    runtime.commit(change, context).await
}

// ---------------------------------------------------------------------------
// answer
// ---------------------------------------------------------------------------

async fn answer(
    runtime: &dyn TaskRuntime,
    message: &AssistantMessage,
    context: &Context,
) -> SessionResult<()> {
    let agent = runtime.agent(context).await?;
    let mut continuation: Option<crate::harness::types::UserInput> = None;
    for handlers in agent_hooks(&agent, GENERATION_TASK_NAME) {
        if continuation.is_some() {
            continue;
        }
        if let HookHandlers::Generation(hooks) = handlers {
            let api = crate::harness::types::RuntimeHookApi { runtime };
            if let Some(next) = hooks.on_yield(message, &api, context).await {
                continuation = Some(next.continue_input);
            }
        }
    }
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let settings = runtime.settings();
    let modes = QueueModes {
        steering_mode: settings.steering_mode,
        follow_up_mode: settings.follow_up_mode,
    };
    let now = runtime.now()?;
    let message = message.clone();
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let message = message.clone();
        let continuation = continuation.clone();
        Box::pin(async move {
            let boundary = prepare_boundary(tx, conversation_id, modes).await?;
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let entry = append_assistant(tx, conversation_id, &message).await?;
            let result = apply_boundary(tx, boundary, BoundaryAt::Final, now).await?;
            if let Some(continue_input) = continuation
                && result.users.is_empty()
                && !result.reset
            {
                let mut draft = EntryDraft::new(USER_ENTRY);
                draft.model = Some(vec![Message::User(UserMessage {
                    role: UserRole::User,
                    content: continue_input,
                    timestamp: now as i64,
                })]);
                tx.append_entry(conversation_id, draft).await?;
                let next = create_generation(
                    tx,
                    conversation_id,
                    crate::session::TaskTrace::Inherit,
                    Vec::new(),
                )
                .await?;
                hand_over(&live, task_id, next)?;
                let mut state: LiveState = crate::harness::types::read_state(&live)?;
                state.generation = None;
                crate::harness::types::write_state(&live, &state)?;
                return Ok(Some(NextTaskState::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: serde_json::json!({ "entryId": entry.id }),
                    },
                }));
            }
            end_run(
                tx,
                &live,
                task_id,
                SubmissionSettlement::Done { answer: entry.id },
            )
            .await?;
            if !result.users.is_empty() {
                start_run(tx, conversation_id, &live, result.users).await?;
            }
            Ok(Some(NextTaskState::Terminal {
                outcome: TaskOutcome::Completed {
                    result: serde_json::json!({ "entryId": entry.id }),
                },
            }))
        })
    });
    runtime.commit(change, context).await
}

// ---------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn start_tool_round(
    runtime: &dyn TaskRuntime,
    reference: &ModelRef,
    cutoff: EntryId,
    messages: &[Message],
    message: &AssistantMessage,
    calls: &[ToolCall],
    context: &Context,
) -> SessionResult<()> {
    let _ = (reference, cutoff);
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let agent = runtime.agent(context).await?;
    let offered: BTreeSet<String> = get_current_tools(messages)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    let tools = agent.tools;
    let sequential = runtime.settings().tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            offered.contains(&call.name)
                && tools
                    .iter()
                    .find(|tool| tool.name() == call.name)
                    .and_then(|tool| tool.execution_mode)
                    == Some(ToolExecutionMode::Sequential)
        });
    let now = runtime.now()? as i64;
    let message = message.clone();
    let calls = calls.to_vec();
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let message = message.clone();
        let calls = calls.clone();
        let offered = offered.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let entry = append_assistant(tx, conversation_id, &message).await?;
            let mut slots: Vec<ToolSlot> = Vec::new();
            let mut tool_tasks: Vec<TaskId> = Vec::new();
            let mut pending: Vec<String> = Vec::new();
            for call in &calls {
                if !offered.contains(&call.name) {
                    let unavailable = harness_error(
                        "tool_unavailable",
                        &format!("Tool {} is not available", call.name),
                    );
                    let result =
                        append_tool_result(tx, conversation_id, call, &unavailable, now, None)
                            .await?;
                    slots.push(ToolSlot {
                        progress: SlotProgress {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            status: ToolSlotStatus::Done,
                            ..Default::default()
                        },
                        entry: Some(result.id),
                        ..Default::default()
                    });
                    continue;
                }
                if sequential && !tool_tasks.is_empty() {
                    pending.push(call.id.clone());
                    slots.push(ToolSlot {
                        progress: SlotProgress {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            status: ToolSlotStatus::Pending,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                    continue;
                }
                let tool_task =
                    create_tool_task(tx, conversation_id, task_id, entry.id, &call.id).await?;
                tool_tasks.push(tool_task);
                slots.push(ToolSlot {
                    progress: SlotProgress {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        status: ToolSlotStatus::Pending,
                        ..Default::default()
                    },
                    task_id: Some(tool_task),
                    ..Default::default()
                });
            }
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            state.generation = None;
            state.tools = Some(slots);
            crate::harness::types::write_state(&live, &state)?;
            Ok(Some(NextTaskState::Waiting {
                checkpoint: to_checkpoint(GenerationCheckpoint::Tools {
                    assistant: entry.id,
                    tools: tool_tasks.clone(),
                    pending,
                }),
                on: tool_tasks,
                policy: JoinPolicy::AllSettled,
            }))
        })
    });
    runtime.commit(change, context).await
}

async fn tools(
    runtime: &dyn TaskRuntime,
    context: &Context,
    checkpoint: GenerationCheckpoint,
) -> SessionResult<()> {
    let (assistant, tool_tasks, pending) = match checkpoint {
        GenerationCheckpoint::Tools {
            assistant,
            tools,
            pending,
        } => (assistant, tools, pending),
        _ => return Ok(()),
    };
    let Some(next) = pending.first().cloned() else {
        return finish_tool_round(runtime, assistant, &tool_tasks, context).await;
    };
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let rest: Vec<String> = pending[1..].to_vec();
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let next = next.clone();
        let rest = rest.clone();
        let tool_tasks = tool_tasks.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let tool_task =
                create_tool_task(tx, conversation_id, task_id, assistant, &next).await?;
            let mut state: LiveState = crate::harness::types::read_state(&live)?;
            if let Some(slots) = state.tools.as_mut()
                && let Some(slot) = slots
                    .iter_mut()
                    .find(|slot| slot.progress.call_id == next && slot.task_id.is_none())
            {
                slot.task_id = Some(tool_task);
            }
            crate::harness::types::write_state(&live, &state)?;
            let mut all = tool_tasks;
            all.push(tool_task);
            Ok(Some(NextTaskState::Waiting {
                checkpoint: to_checkpoint(GenerationCheckpoint::Tools {
                    assistant,
                    tools: all,
                    pending: rest,
                }),
                on: vec![tool_task],
                policy: JoinPolicy::AllSettled,
            }))
        })
    });
    runtime.commit(change, context).await
}

async fn finish_tool_round(
    runtime: &dyn TaskRuntime,
    assistant: EntryId,
    tool_tasks: &[TaskId],
    context: &Context,
) -> SessionResult<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let outcomes = runtime.outcomes(tool_tasks.to_vec(), context).await?;
    let mut control_map: BTreeMap<TaskId, ToolControl> = BTreeMap::new();
    for (id, outcome) in tool_tasks.iter().zip(outcomes.iter()) {
        if let TaskOutcome::Completed { result } = outcome
            && let Some(control) = result.get("control")
            && let Ok(control) = serde_json::from_value::<ToolControl>(control.clone())
        {
            control_map.insert(*id, control);
        }
    }
    let live = runtime
        .snapshot(&LIVE_DOC, DocOwner::Conversation(conversation_id), context)
        .await?;
    let slots: Vec<ToolSlot> = live
        .and_then(|value| serde_json::from_value::<LiveState>(JsonValue::Object(value)).ok())
        .and_then(|state| state.tools)
        .unwrap_or_default();
    let results: Vec<EntryId> = slots.iter().filter_map(|slot| slot.entry).collect();
    let agent = runtime.agent(context).await?;
    for handlers in agent_hooks(&agent, GENERATION_TASK_NAME) {
        if let HookHandlers::Generation(hooks) = handlers {
            let api = crate::harness::types::RuntimeHookApi { runtime };
            hooks.after_tools(assistant, &results, &api, context).await;
        }
    }
    let terminate = !slots.is_empty()
        && slots.iter().all(|slot| {
            slot.task_id
                .and_then(|id| control_map.get(&id))
                .and_then(|control| control.terminate)
                == Some(true)
        });
    let mut added: Vec<String> = Vec::new();
    let mut handoff: Option<String> = None;
    for id in tool_tasks {
        if let Some(control) = control_map.get(id) {
            if let Some(names) = &control.add_tools {
                added.extend(names.iter().cloned());
            }
            if control.handoff.is_some() {
                handoff = control.handoff.clone();
            }
        }
    }
    let settings = runtime.settings();
    let modes = QueueModes {
        steering_mode: settings.steering_mode,
        follow_up_mode: settings.follow_up_mode,
    };
    let now = runtime.now()?;
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let added = added.clone();
        let handoff = handoff.clone();
        Box::pin(async move {
            let mut boundary = prepare_boundary(tx, conversation_id, modes).await?;
            if !added.is_empty() {
                add_tools(tx, conversation_id, &added).await?;
            }
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            if terminate || handoff.is_some() {
                if let Some(handoff) = handoff {
                    let mut draft = EntryDraft::new(RESET_ENTRY);
                    draft.head = Some(EntryHead::This);
                    draft.model = Some(vec![Message::User(UserMessage {
                        role: UserRole::User,
                        content: rpi_ai::types::UserContent::Text(handoff),
                        timestamp: now as i64,
                    })]);
                    let record = tx.append_entry(conversation_id, draft).await?;
                    boundary.head = Some(record.id);
                }
                let result = apply_boundary(tx, boundary, BoundaryAt::Final, now).await?;
                end_run(
                    tx,
                    &live,
                    task_id,
                    SubmissionSettlement::Done { answer: assistant },
                )
                .await?;
                if !result.users.is_empty() {
                    start_run(tx, conversation_id, &live, result.users).await?;
                }
            } else {
                let result = apply_boundary(tx, boundary, BoundaryAt::PostTools, now).await?;
                if result.reset {
                    end_run(
                        tx,
                        &live,
                        task_id,
                        SubmissionSettlement::Unanswered {
                            reason: "reset".to_string(),
                            detail: None,
                        },
                    )
                    .await?;
                    if !result.users.is_empty() {
                        start_run(tx, conversation_id, &live, result.users).await?;
                    }
                } else {
                    let mut state: LiveState = crate::harness::types::read_state(&live)?;
                    state.tools = None;
                    if let Some(run) = state.run.as_mut()
                        && run.task_id == task_id
                    {
                        run.inputs.extend(result.users);
                    }
                    crate::harness::types::write_state(&live, &state)?;
                    let next = create_generation(
                        tx,
                        conversation_id,
                        crate::session::TaskTrace::Inherit,
                        Vec::new(),
                    )
                    .await?;
                    hand_over(&live, task_id, next)?;
                    let mut state: LiveState = crate::harness::types::read_state(&live)?;
                    state.generation = None;
                    crate::harness::types::write_state(&live, &state)?;
                }
            }
            Ok(Some(NextTaskState::Terminal {
                outcome: TaskOutcome::Completed {
                    result: serde_json::json!({ "entryId": assistant }),
                },
            }))
        })
    });
    runtime.commit(change, context).await
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

async fn append_assistant(
    tx: &mut Transaction,
    conversation_id: ConversationId,
    message: &AssistantMessage,
) -> SessionResult<crate::types::EntryRecord> {
    record_usage(
        tx,
        conversation_id,
        UsageBucket::Models,
        &format!("{}/{}", message.provider, message.model),
        &message.usage,
    )
    .await?;
    let mut draft = EntryDraft::new(ASSISTANT_ENTRY);
    draft.model = Some(vec![Message::Assistant(message.clone())]);
    tx.append_entry(conversation_id, draft).await
}

async fn fail_model_error(
    runtime: &dyn TaskRuntime,
    text: String,
    context: &Context,
) -> SessionResult<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let settings = runtime.settings();
    let after = settings.after_failed_run;
    let modes = QueueModes {
        steering_mode: settings.steering_mode,
        follow_up_mode: settings.follow_up_mode,
    };
    let now = runtime.now()?;
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let text = text.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let prepared = if after == AfterFailedRun::Continue {
                Some(prepare_boundary(tx, conversation_id, modes).await?)
            } else {
                None
            };
            end_run(
                tx,
                &live,
                task_id,
                SubmissionSettlement::Unanswered {
                    reason: "model_error".to_string(),
                    detail: Some(JsonValue::String(text.clone())),
                },
            )
            .await?;
            apply_after_failed_run(tx, conversation_id, &live, after, modes, now, prepared).await?;
            Ok(Some(NextTaskState::Terminal {
                outcome: TaskOutcome::Failed {
                    error: TaskOutcomeError {
                        message: text,
                        detail: Some(serde_json::json!({ "reason": "model_error" })),
                    },
                    result: None,
                },
            }))
        })
    });
    runtime.commit(change, context).await
}

async fn fail_no_model(
    runtime: &dyn TaskRuntime,
    reference: Option<&ModelRef>,
    context: &Context,
) -> SessionResult<()> {
    let message = match reference {
        Some(reference) => format!(
            "Model {}/{} is not available",
            reference.provider, reference.model_id
        ),
        None => "No model is configured".to_string(),
    };
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let settings = runtime.settings();
    let after = settings.after_failed_run;
    let modes = QueueModes {
        steering_mode: settings.steering_mode,
        follow_up_mode: settings.follow_up_mode,
    };
    let now = runtime.now()?;
    let change: TaskCommitFn = Arc::new(move |tx, _current| {
        let message = message.clone();
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let prepared = if after == AfterFailedRun::Continue {
                Some(prepare_boundary(tx, conversation_id, modes).await?)
            } else {
                None
            };
            end_run(
                tx,
                &live,
                task_id,
                SubmissionSettlement::Unanswered {
                    reason: "no_model".to_string(),
                    detail: None,
                },
            )
            .await?;
            apply_after_failed_run(tx, conversation_id, &live, after, modes, now, prepared).await?;
            Ok(Some(NextTaskState::Terminal {
                outcome: TaskOutcome::Failed {
                    error: TaskOutcomeError {
                        message,
                        detail: Some(serde_json::json!({ "reason": "no_model" })),
                    },
                    result: None,
                },
            }))
        })
    });
    runtime.commit(change, context).await
}

/// What a failed run does with the inputs queued behind it (`afterFailedRun`).
async fn apply_after_failed_run(
    tx: &mut Transaction,
    conversation_id: ConversationId,
    live: &DocDraft,
    after: AfterFailedRun,
    modes: QueueModes,
    now: f64,
    prepared: Option<crate::harness::inbox::Boundary>,
) -> SessionResult<()> {
    match after {
        AfterFailedRun::Keep => Ok(()),
        AfterFailedRun::Withdraw => {
            let inbox = tx
                .doc(&INBOX_DOC, DocOwner::Conversation(conversation_id), None)
                .await?;
            let mut state: InboxState = read_state(&inbox)?;
            let mut kept = Vec::new();
            for item in state.items {
                match item {
                    InboxItem::Steer { id, .. } | InboxItem::FollowUp { id, .. } => {
                        tx.settle_submission(
                            id,
                            SubmissionSettlement::Unanswered {
                                reason: "run_failed".to_string(),
                                detail: None,
                            },
                        )?;
                    }
                    write => kept.push(write),
                }
            }
            state.items = kept;
            write_state(&inbox, &state)
        }
        AfterFailedRun::Continue => {
            let boundary = match prepared {
                Some(boundary) => boundary,
                None => prepare_boundary(tx, conversation_id, modes).await?,
            };
            let pending: InboxState = read_state(&boundary.inbox)?;
            if pending.items.is_empty() {
                return Ok(());
            }
            let result = apply_boundary(tx, boundary, BoundaryAt::Final, now).await?;
            if !result.users.is_empty() {
                start_run(tx, conversation_id, live, result.users).await?;
            }
            Ok(())
        }
    }
}

/// Error text for a generation whose blocking compaction ended without completing.
fn compaction_failure(outcome: &TaskOutcome) -> String {
    match outcome {
        TaskOutcome::Failed { error, .. } | TaskOutcome::Faulted { error } => {
            format!("Compaction failed: {}", error.message)
        }
        TaskOutcome::Orphaned { reason } => format!("Compaction failed: {reason}"),
        TaskOutcome::Aborted { .. } => "Compaction was aborted".to_string(),
        TaskOutcome::Completed { .. } => "Compaction failed".to_string(),
    }
}

/// Create a tool task for call `call_id`, owned by the generation.
async fn create_tool_task(
    tx: &mut Transaction,
    conversation_id: ConversationId,
    owner: TaskId,
    assistant: EntryId,
    call_id: &str,
) -> SessionResult<TaskId> {
    tx.create_task(TaskCreate {
        kind: TOOL_TASK_NAME.to_string(),
        version: 2,
        input: serde_json::json!({ "kind": "model", "assistant": assistant, "callId": call_id }),
        checkpoint: serde_json::json!({ "phase": "call" }),
        ownership: TaskOwnership::Task(owner),
        conversation_id: Some(conversation_id),
        background: false,
        abandon_on_restart: false,
        trace: Default::default(),
        links: Vec::new(),
    })
    .await
}

/// Start a run for `inputs`, placed input submissions: a new generation takes
/// `pi.live.run`.
pub async fn start_run(
    tx: &mut Transaction,
    conversation_id: ConversationId,
    live: &DocDraft,
    inputs: Vec<SubmissionId>,
) -> SessionResult<()> {
    let (trace, links) = tx.task_trace(&inputs).await?;
    let task_id = create_generation(tx, conversation_id, trace, links).await?;
    let mut state: LiveState = crate::harness::types::read_state(live)?;
    state.run = Some(crate::harness::live::LiveRun { task_id, inputs });
    crate::harness::types::write_state(live, &state)
}

/// A generation owned by its conversation.
async fn create_generation(
    tx: &mut Transaction,
    conversation_id: ConversationId,
    trace: crate::session::TaskTrace,
    links: Vec<crate::chord::telemetry::TelemetryCarrier>,
) -> SessionResult<TaskId> {
    tx.create_task(TaskCreate {
        kind: GENERATION_TASK_NAME.to_string(),
        version: 1,
        input: serde_json::json!({}),
        checkpoint: serde_json::json!({ "phase": "prepare", "attempt": 1 }),
        ownership: TaskOwnership::Conversation(conversation_id),
        conversation_id: Some(conversation_id),
        background: false,
        abandon_on_restart: false,
        trace,
        links,
    })
    .await
}

/// Hand run control from `from` to `to`; the run's inputs move with it.
pub fn hand_over(live: &DocDraft, from: TaskId, to: TaskId) -> SessionResult<()> {
    let mut state: LiveState = crate::harness::types::read_state(live)?;
    if let Some(run) = state.run.as_mut()
        && run.task_id == from
    {
        run.task_id = to;
        crate::harness::types::write_state(live, &state)?;
    }
    Ok(())
}

fn to_checkpoint(checkpoint: GenerationCheckpoint) -> JsonValue {
    serde_json::to_value(checkpoint).unwrap_or(JsonValue::Null)
}
