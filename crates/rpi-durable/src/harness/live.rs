//! Built-in live conversation state (port of `packages/durable/src/harness/live.ts`
//! @ `4a080252f`).
//!
//! The `pi.live` document carries run control (`run`), the current generation's
//! presentation, the current tool round, and live compaction statuses. Its
//! checkpoint predicate keeps a complete base whenever nothing runs, so the delta
//! chain spans at most one generation or one overlapping tool round.

use std::sync::{Arc, LazyLock};

use rpi_ai::types::{AssistantMessage, Message, StopReason};
use serde::{Deserialize, Serialize};

use crate::chord::json::{JsonObject, JsonValue};
use crate::documents::{DocDefinition, DocScope, DocToken, define_doc};
use crate::entries::ASSISTANT_ENTRY;
use crate::harness::types::{CompactionReason, ToolDiagnostic, read_state, write_state};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::ids::{EntryId, SubmissionId, TaskId};
use crate::session::{
    DocDraft, DocOwner, EntryDraft, SessionResult, SubmissionSettlement, Transaction,
};
use crate::types::{DocumentFork, DocumentHistory, TaskOutcomeError, TaskRecord};

/// Built-in task kind that can own `pi.live.run`.
pub const RUN_TASK_KINDS: [&str; 1] = ["pi.generation"];
/// Built-in tool task kind.
pub const TOOL_TASK_KIND: &str = "pi.tool";
/// Built-in compaction task kind.
pub const COMPACTION_TASK_KIND: &str = "pi.compaction";

/// What a running tool call publishes, shared by model-issued and nested calls.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotProgress {
    /// Provider call ID.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// Presentation status.
    pub status: ToolSlotStatus,
    /// Retained running output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Bytes the bounds dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
    /// Lines the bounds dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_lines: Option<u64>,
    /// Last `details()` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    /// Diagnostics recorded through `api.diagnostic()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
}

/// Presentation of one tool call of the current round.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSlot {
    /// Shared running presentation.
    #[serde(flatten)]
    pub progress: SlotProgress,
    /// Owning tool task; absent for a call not started yet and for an unoffered call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Result entry once done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
}

/// How a nested call ended: an error flag, its execution time, its spend, and up
/// to 500 characters of error text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSummary {
    /// Whether the call ended as an error.
    pub is_error: bool,
    /// Execution time of the attempt that produced the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Spend of the execution itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<rpi_ai::types::Usage>,
    /// Error text, bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Presentation of one nested call: a call a running tool made through
/// `executeTool()`. `call_id` is `<parentCallId>/<key>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSlot {
    /// Shared running presentation.
    #[serde(flatten)]
    pub progress: SlotProgress,
    /// Call ID of the calling tool.
    pub parent_call_id: String,
    /// Calling tool task.
    pub parent_task_id: TaskId,
    /// The nested call's tool task.
    pub task_id: TaskId,
    /// What the call was made with; replaced by the arguments it runs with once
    /// it starts executing.
    pub arguments: JsonObject,
    /// Once done: how the call ended, for a status line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<NestedToolSummary>,
}

/// Status of one [`ToolSlot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolSlotStatus {
    /// Not started yet.
    Pending,
    /// Running.
    Running,
    /// Terminal.
    #[default]
    Done,
}

/// Durable backoff before the next attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryStatus {
    /// Wall-clock time of the next attempt.
    pub at: f64,
    /// Failure text.
    pub error: String,
}

/// Presentation of one live compaction task (spec §8.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStatus {
    /// Compaction task.
    pub task_id: TaskId,
    /// Why it runs.
    pub reason: CompactionReason,
    /// Whether a generation waits for it.
    pub blocking: bool,
    /// Attempt number.
    pub attempt: u64,
    /// Durable backoff before the next summarization attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryStatus>,
}

/// The newest compaction that failed since a compaction last produced a summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFailure {
    /// Compaction task.
    pub task_id: TaskId,
    /// Why it ran.
    pub reason: CompactionReason,
    /// The failed or faulted outcome's error, or why the scheduler orphaned it.
    pub error: String,
}

/// Run control and presentation of the current generation and tool round.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    /// Run control: the task that settles the run's inputs, and those inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<LiveRun>,
    /// Presentation of the current generation attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<LiveGeneration>,
    /// The current tool round in call order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSlot>>,
    /// Nested calls of running tool calls, in creation order, so a parent
    /// precedes its children and task IDs ascend. A call's nested slots are
    /// removed when the call settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_tools: Option<Vec<NestedToolSlot>>,
    /// Live compaction tasks in task ID order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions: Option<Vec<CompactionStatus>>,
    /// The newest compaction failure, kept until a summary is produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_failure: Option<CompactionFailure>,
}

/// Run control of one busy conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRun {
    /// Task that settles the run's inputs.
    pub task_id: TaskId,
    /// Inputs the run consumes.
    pub inputs: Vec<SubmissionId>,
}

/// Presentation of the current generation attempt.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveGeneration {
    /// Attempt number.
    pub attempt: u64,
    /// Committed throttled partial of the in-flight response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<JsonValue>,
    /// Durable backoff before the next attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryStatus>,
    /// Provider-side deferred response being polled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredStatus>,
}

/// Provider-side deferred poll state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredStatus {
    /// Wall-clock time of the next poll.
    pub poll_at: f64,
}

fn initial() -> JsonObject {
    match serde_json::to_value(LiveState::default()) {
        Ok(JsonValue::Object(object)) => object,
        // Invariant: a `LiveState` always serializes to a JSON object.
        _ => JsonObject::new(),
    }
}

/// Built-in live document (`pi.live`), conversation scope, history latest.
pub static LIVE_DOC: LazyLock<DocToken> = LazyLock::new(|| {
    define_doc(DocDefinition {
        kind: "pi.live".to_string(),
        version: 1,
        scope: DocScope::Conversation,
        history: Some(DocumentHistory::Latest),
        fork: Some(DocumentFork::Initial),
        family: false,
        initial: Arc::new(|_seed| initial()),
        migrate: None,
        // A complete base whenever nothing runs (spec §8.2): no generation and no
        // running tool slot.
        checkpoint_when: Some(Arc::new(|value, _ops, _revision| {
            let no_generation = value.get("generation").is_none();
            let no_running = ["tools", "nestedTools"].iter().all(|field| {
                value
                    .get(*field)
                    .and_then(JsonValue::as_array)
                    .map(|slots| {
                        slots.iter().all(|slot| {
                            slot.get("status").and_then(JsonValue::as_str) != Some("running")
                        })
                    })
                    .unwrap_or(true)
            });
            no_generation && no_running
        })),
    })
});

fn conversation_owner(record: &TaskRecord) -> DocOwner<'_> {
    DocOwner::Conversation(record.conversation_id)
}

/// End the run owned by `task_id`: settle each of its inputs and remove `run`.
/// Always removes `generation` and `tools`.
pub async fn end_run(
    tx: &mut Transaction,
    live: &DocDraft,
    task_id: TaskId,
    settlement: SubmissionSettlement,
) -> SessionResult<()> {
    let mut state: LiveState = read_state(live)?;
    if state.run.as_ref().map(|run| run.task_id) == Some(task_id)
        && let Some(run) = state.run.take()
    {
        for id in run.inputs {
            tx.settle_submission(id, settlement.clone())?;
        }
    }
    state.generation = None;
    state.tools = None;
    state.nested_tools = None;
    write_state(live, &state)
}

/// Add the status of a compaction task; statuses stay in task ID order.
pub fn add_compaction_status(live: &DocDraft, status: CompactionStatus) -> SessionResult<()> {
    let mut state: LiveState = read_state(live)?;
    let mut list = state.compactions.take().unwrap_or_default();
    list.push(status);
    list.sort_by_key(|status| status.task_id);
    state.compactions = Some(list);
    write_state(live, &state)
}

/// The status of compaction task `task_id`, if listed.
pub fn compaction_status(
    live: &DocDraft,
    task_id: TaskId,
) -> SessionResult<Option<CompactionStatus>> {
    let state: LiveState = read_state(live)?;
    Ok(state
        .compactions
        .unwrap_or_default()
        .into_iter()
        .find(|status| status.task_id == task_id))
}

/// Remove the status of compaction task `task_id` and record that it failed.
pub fn fail_compaction_status(
    live: &DocDraft,
    task_id: TaskId,
    error: String,
) -> SessionResult<()> {
    let mut state: LiveState = read_state(live)?;
    if let Some(status) = state
        .compactions
        .as_ref()
        .and_then(|statuses| statuses.iter().find(|status| status.task_id == task_id))
    {
        state.compaction_failure = Some(CompactionFailure {
            task_id: status.task_id,
            reason: status.reason,
            error,
        });
    }
    if let Some(list) = state.compactions.as_mut() {
        list.retain(|status| status.task_id != task_id);
        if list.is_empty() {
            state.compactions = None;
        }
    }
    write_state(live, &state)
}

/// Clear the recorded compaction failure once a summary is produced.
pub fn clear_compaction_failure(live: &DocDraft) -> SessionResult<()> {
    let mut state: LiveState = read_state(live)?;
    if state.compaction_failure.is_some() {
        state.compaction_failure = None;
        write_state(live, &state)?;
    }
    Ok(())
}

/// Remove the status of compaction task `task_id`, and the list once empty.
pub fn remove_compaction_status(live: &DocDraft, task_id: TaskId) -> SessionResult<()> {
    let mut state: LiveState = read_state(live)?;
    let Some(list) = state.compactions.as_mut() else {
        return Ok(());
    };
    list.retain(|status| status.task_id != task_id);
    if list.is_empty() {
        state.compactions = None;
    }
    write_state(live, &state)
}

/// The slot of tool task `task_id` in the current round, if the round lists it.
pub fn tool_slot(live: &DocDraft, task_id: TaskId) -> SessionResult<Option<ToolSlot>> {
    let state: LiveState = read_state(live)?;
    Ok(state
        .tools
        .unwrap_or_default()
        .into_iter()
        .find(|slot| slot.task_id == Some(task_id)))
}

/// One live slot of either kind.
#[derive(Debug)]
pub enum LiveSlot<'a> {
    /// A model-issued call of the current round.
    Model(&'a mut ToolSlot),
    /// A nested call of a running tool.
    Nested(&'a mut NestedToolSlot),
}

impl<'a> LiveSlot<'a> {
    /// Shared running presentation.
    pub fn progress(&mut self) -> &mut SlotProgress {
        match self {
            LiveSlot::Model(slot) => &mut slot.progress,
            LiveSlot::Nested(slot) => &mut slot.progress,
        }
    }

    /// Whether this is a nested call's slot.
    pub fn is_nested(&self) -> bool {
        matches!(self, LiveSlot::Nested(_))
    }

    /// The slot's task ID.
    pub fn task_id(&self) -> TaskId {
        match self {
            LiveSlot::Model(slot) => slot.task_id.unwrap_or(TaskId::from_number(0)),
            LiveSlot::Nested(slot) => slot.task_id,
        }
    }

    /// Mark the slot done and clear its running output; the caller sets a
    /// model-issued call's result `entry` or a nested call's `summary`.
    pub fn finish(&mut self) {
        self.progress().status = ToolSlotStatus::Done;
        clear_progress(self.progress());
    }
}

/// The slot of tool task `task_id`, model-issued or nested, mutably.
pub fn tool_slot_mut<'a>(state: &'a mut LiveState, task_id: TaskId) -> Option<LiveSlot<'a>> {
    let in_tools = state
        .tools
        .as_ref()
        .map(|tools| tools.iter().any(|slot| slot.task_id == Some(task_id)))
        .unwrap_or(false);
    if in_tools {
        let tools = state.tools.as_mut()?;
        let slot = tools
            .iter_mut()
            .find(|slot| slot.task_id == Some(task_id))?;
        return Some(LiveSlot::Model(slot));
    }
    let nested = state.nested_tools.as_mut()?;
    // Task IDs ascend: each slot is appended in the commit that creates its
    // task, task IDs ascend in commit order, and removal keeps the order.
    let index = nested.partition_point(|slot| slot.task_id < task_id);
    match nested.get_mut(index) {
        Some(slot) if slot.task_id == task_id => Some(LiveSlot::Nested(slot)),
        _ => None,
    }
}

/// A read-only snapshot of a slot's running presentation.
#[derive(Debug, Clone, Default)]
pub struct SlotSnapshot {
    /// Retained output.
    pub output: Option<String>,
    /// Dropped bytes.
    pub dropped_bytes: Option<u64>,
    /// Dropped lines.
    pub dropped_lines: Option<u64>,
    /// Last details.
    pub details: Option<JsonValue>,
    /// Diagnostics.
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
}

impl From<&SlotProgress> for SlotSnapshot {
    fn from(progress: &SlotProgress) -> Self {
        Self {
            output: progress.output.clone(),
            dropped_bytes: progress.dropped_bytes,
            dropped_lines: progress.dropped_lines,
            details: progress.details.clone(),
            diagnostics: progress.diagnostics.clone(),
        }
    }
}

/// The read-only snapshot of tool task `task_id`'s slot, model-issued or nested.
pub fn tool_slot_snapshot(live: &DocDraft, task_id: TaskId) -> SessionResult<Option<SlotSnapshot>> {
    let state: LiveState = read_state(live)?;
    if let Some(slot) = state
        .tools
        .as_ref()
        .and_then(|tools| tools.iter().find(|slot| slot.task_id == Some(task_id)))
    {
        return Ok(Some((&slot.progress).into()));
    }
    Ok(state
        .nested_tools
        .as_ref()
        .and_then(|nested| nested.iter().find(|slot| slot.task_id == task_id))
        .map(|slot| (&slot.progress).into()))
}

/// Remove the nested slots below tool task `task_id`, transitively; the list
/// holds parents before their children.
pub fn remove_nested_slots(state: &mut LiveState, task_id: TaskId) {
    let Some(nested) = state.nested_tools.as_mut() else {
        return;
    };
    let mut removed: std::collections::BTreeSet<TaskId> = std::collections::BTreeSet::new();
    removed.insert(task_id);
    for slot in nested.iter() {
        if removed.contains(&slot.parent_task_id) {
            removed.insert(slot.task_id);
        }
    }
    nested.retain(|slot| !removed.contains(&slot.parent_task_id));
    if nested.is_empty() {
        state.nested_tools = None;
    }
}

/// Mark a slot done: the result entry, if any, now carries its running output.
pub fn finish_slot(slot: &mut ToolSlot, entry: Option<EntryId>) {
    slot.progress.status = ToolSlotStatus::Done;
    if entry.is_some() {
        slot.entry = entry;
    }
    clear_progress(&mut slot.progress);
}

/// Remove what a tool published while running; its result entry or a rerun
/// replaces it.
pub fn clear_progress(progress: &mut SlotProgress) {
    progress.output = None;
    progress.dropped_bytes = None;
    progress.dropped_lines = None;
    progress.details = None;
    progress.diagnostics = None;
}

/// Terminal outcome the scheduler writes itself.
#[derive(Debug, Clone, PartialEq)]
pub enum SchedulerOutcome {
    /// A contract failure detected by the run loop.
    Faulted {
        /// Failure snapshot.
        error: TaskOutcomeError,
    },
    /// A task no registered definition can take.
    Orphaned {
        /// Why the task is orphaned.
        reason: String,
    },
}

/// Harness cleanup for a terminal outcome the scheduler writes itself (`faulted`
/// or `orphaned`).
///
/// A run task ends its run (after turning a committed partial into an aborted
/// assistant entry); a tool task's slot is marked done without an entry; a
/// compaction task's status is removed. Other kinds are ignored so this never
/// creates `pi.live` elsewhere.
pub async fn settle_scheduler_outcome(
    tx: &mut Transaction,
    record: &TaskRecord,
    outcome: &SchedulerOutcome,
) -> SessionResult<()> {
    if record.kind == TOOL_TASK_KIND {
        let live = tx.doc(&LIVE_DOC, conversation_owner(record), None).await?;
        let mut state: LiveState = read_state(&live)?;
        if let Some(mut slot) = tool_slot_mut(&mut state, record.id) {
            slot.finish();
        }
        remove_nested_slots(&mut state, record.id);
        write_state(&live, &state)?;
        return Ok(());
    }
    if record.kind == COMPACTION_TASK_KIND {
        let live = tx.doc(&LIVE_DOC, conversation_owner(record), None).await?;
        let error = match outcome {
            SchedulerOutcome::Faulted { error } => error.message.clone(),
            SchedulerOutcome::Orphaned { reason } => reason.clone(),
        };
        return fail_compaction_status(&live, record.id, error);
    }
    if !RUN_TASK_KINDS.contains(&record.kind.as_str()) {
        return Ok(());
    }
    let live = tx.doc(&LIVE_DOC, conversation_owner(record), None).await?;
    let state: LiveState = read_state(&live)?;
    if state.run.as_ref().map(|run| run.task_id) != Some(record.id) {
        return Ok(());
    }
    convert_partial(tx, &live).await?;
    let settlement = match outcome {
        SchedulerOutcome::Faulted { error } => SubmissionSettlement::Unanswered {
            reason: "faulted".to_string(),
            detail: Some(JsonValue::String(error.message.clone())),
        },
        SchedulerOutcome::Orphaned { reason } => SubmissionSettlement::Unanswered {
            reason: reason.clone(),
            detail: None,
        },
    };
    end_run(tx, &live, record.id, settlement).await
}

/// Append a committed partial left by an interrupted, aborted, faulted, or
/// orphaned attempt as an aborted assistant entry.
///
/// Upstream defines this in `generation.ts`; the Rust port keeps it here so the
/// scheduler-outcome cleanup and the generation abort handler share one
/// implementation.
pub async fn convert_partial(tx: &mut Transaction, live: &DocDraft) -> SessionResult<()> {
    let state: LiveState = read_state(live)?;
    let Some(partial) = state.generation.and_then(|generation| generation.message) else {
        return Ok(());
    };
    let mut message: AssistantMessage = match serde_json::from_value(partial) {
        Ok(message) => message,
        Err(error) => return Err(crate::session::SessionError::message(error.to_string())),
    };
    message.stop_reason = StopReason::Aborted;
    let conversation_id = conversation_of(live)?;
    record_usage(
        tx,
        conversation_id,
        UsageBucket::Models,
        &format!("{}/{}", message.provider, message.model),
        &message.usage,
    )
    .await?;
    let mut draft = EntryDraft::new(ASSISTANT_ENTRY);
    draft.model = Some(vec![Message::Assistant(message)]);
    tx.append_entry(conversation_id, draft).await?;
    Ok(())
}

/// The conversation a draft belongs to, decoded from its cache address identity.
fn conversation_of(live: &DocDraft) -> SessionResult<crate::ids::ConversationId> {
    // Address ids are `["kind","conversation",<id>,null]`; the conversation id is
    // the third element. Parsing the address keeps this helper independent of the
    // caller's scope.
    let parsed: JsonValue = serde_json::from_str(live.address_id())
        .map_err(|error| crate::session::SessionError::message(error.to_string()))?;
    let id = parsed
        .as_array()
        .and_then(|parts| parts.get(2))
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| {
            crate::session::SessionError::message("live document has no conversation")
        })?;
    Ok(crate::ids::id_from_number::<crate::ids::Conversation>(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_slot_clears_progress_and_keeps_entry_when_absent() {
        let mut slot = ToolSlot {
            progress: SlotProgress {
                call_id: "c".to_string(),
                name: "read".to_string(),
                status: ToolSlotStatus::Running,
                output: Some("out".to_string()),
                ..SlotProgress::default()
            },
            entry: Some(crate::ids::id_from_number::<crate::ids::Entry>(9)),
            ..ToolSlot::default()
        };
        finish_slot(&mut slot, None);
        assert_eq!(slot.progress.status, ToolSlotStatus::Done);
        assert_eq!(slot.progress.output, None);
        assert!(slot.entry.is_some());
    }

    #[test]
    fn live_state_default_round_trips() {
        let value = serde_json::to_value(LiveState::default()).unwrap();
        assert_eq!(value, serde_json::json!({}));
    }

    #[tokio::test]
    async fn end_run_settles_inputs_and_clears_presentation() {
        use crate::ids::{Task, id_from_number};
        use crate::session::{ConversationOwnership, SubmissionCreate, commit_fn, create_session};
        use crate::storage::MemoryStorage;

        let session = create_session(Arc::new(MemoryStorage::new()), None);
        let context = crate::chord::context::BACKGROUND_CONTEXT.clone();
        let task_id = id_from_number::<Task>(7);
        let (conversation, submission) = session
            .commit(
                commit_fn(move |tx| {
                    Box::pin(async move {
                        let record = tx
                            .create_conversation(ConversationOwnership::Ownerless)
                            .await?;
                        let submission = tx
                            .create_submission(SubmissionCreate::Input {
                                conversation_id: record.id,
                                request_id: None,
                            })
                            .await?;
                        let live = tx
                            .doc(&LIVE_DOC, DocOwner::Conversation(record.id), None)
                            .await?;
                        write_state(
                            &live,
                            &LiveState {
                                run: Some(LiveRun {
                                    task_id,
                                    inputs: vec![submission.id()],
                                }),
                                generation: Some(LiveGeneration {
                                    attempt: 1,
                                    message: None,
                                    retry: None,
                                    deferred: None,
                                }),
                                ..LiveState::default()
                            },
                        )?;
                        end_run(
                            tx,
                            &live,
                            task_id,
                            SubmissionSettlement::Unanswered {
                                reason: "done".to_string(),
                                detail: None,
                            },
                        )
                        .await?;
                        Ok((record.id, submission.id()))
                    })
                }),
                &context,
            )
            .await
            .unwrap();
        let record = session
            .storage()
            .submission(submission, &context)
            .await
            .unwrap()
            .expect("submission");
        assert_eq!(record.status(), "unanswered");
        let live = session
            .snapshot(&LIVE_DOC, DocOwner::Conversation(conversation), &context)
            .await
            .unwrap()
            .expect("live");
        let state: LiveState = serde_json::from_value(JsonValue::Object(live)).unwrap();
        assert!(state.run.is_none());
        assert!(state.generation.is_none());
    }
}
