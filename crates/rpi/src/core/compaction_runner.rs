//! Port of the compaction wiring of
//! `packages/coding-agent/src/core/agent-session.ts` @ pi 0.82.1 (2efa728),
//! updated to 4181f66 (v0.84.1+) for the length-stop recovery chain
//! (32850ef7c/e56893f4c/8eda4f5b2/3852cb2b8 — see v0.11 T23):
//! `compact()` (:1783-1925), `_checkCompaction` (:1953-2042),
//! `_runAutoCompaction` (:2047-2215), and `_summarizationRetryCallbacks`
//! (:2646-2669).
//!
//! Owns the two trigger paths (post-`agent_end` and pre-prompt share
//! [`CompactionRunner::check_compaction`]; the caller decides whether
//! `agent.continue` follows), overflow recovery, and the manual compaction
//! API. The pure compaction logic lives in `rpi_agent::compaction`; session
//! I/O goes through [`SessionManager`].
//!
//! Intentional differences:
//! - The `signal` field of `session_before_compact` does not cross the
//!   extension boundary (not serializable); extension cancellation rides the
//!   `{cancel: true}` result instead.
//! - Unknown/raw session entries are dropped when converting the branch to
//!   typed entries; they carry zero context, so `prepareCompaction` sees the
//!   same message stream either way.
//! - `this.model` is guaranteed by the caller, so the runner holds a
//!   [`Model`] directly; the `formatNoModelSelectedMessage()` branch is
//!   unreachable here.
//! - Auth (`_getSummarizationRequestAuth`) is out of scope for the faux-first
//!   path: `apiKey`/`headers`/`env` ride [`SummarizationArgs`] and default to
//!   `None`, exactly what a custom `streamFn` receives upstream.

use std::sync::{Arc, Mutex, MutexGuard};

use rpi_agent::compaction::{
    calculate_context_tokens, compact as run_compact, estimate_context_tokens,
    estimate_messages_tokens, prepare_compaction, should_compact, CompactionResult,
    CompactionSettings, SummarizationArgs,
};
use rpi_agent::session::{get_latest_compaction_entry, parse_iso8601_ms, SessionEntry};
use rpi_agent::types::ThinkingLevel;
use rpi_agent::{Agent, StreamFn};
use rpi_ai::types::{AssistantMessage, Model, StopReason};
use rpi_ai::utils::overflow::{is_context_overflow, is_recoverable_length};
use rpi_ai::utils::retry::{RetryCallbacks, RetryPolicy};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::core::auth_guidance::format_no_model_selected_message;
use crate::core::session_manager::SessionManager;
use crate::error::RpiError;

/// `"manual" | "threshold" | "overflow"` (agent-session.ts:152).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

impl CompactionReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CompactionReason::Manual => "manual",
            CompactionReason::Threshold => "threshold",
            CompactionReason::Overflow => "overflow",
        }
    }
}

/// `source` of `summarization_retry_attempt_start` (agent-session.ts:173-178).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "camelCase")]
pub enum RetrySource {
    BranchSummary,
    Compaction { reason: CompactionReason },
}

/// The compaction slice of `AgentSessionEvent` (agent-session.ts:152-179),
/// serialized exactly like upstream (`camelCase` fields, `type` tag).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CompactionEvent {
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        /// `Box` keeps the enum small; serde is transparent to it.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Box<CompactionResult>>,
        aborted: bool,
        will_retry: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    SummarizationRetryScheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    SummarizationRetryAttemptStart {
        #[serde(flatten)]
        source: RetrySource,
    },
    SummarizationRetryFinished,
}

/// Event sink: the session layer (T10) forwards these into its own event
/// stream; tests capture them verbatim for parity comparison.
pub type CompactionEventSink = Arc<dyn Fn(CompactionEvent) + Send + Sync>;

/// The bare error text upstream puts into `errorMessage` (`error.message`),
/// without the `RpiError` Display prefix.
fn raw_error_message(error: &RpiError) -> String {
    match error {
        RpiError::Session(message) => message.clone(),
        other => other.to_string(),
    }
}

/// Compaction trigger wiring (agent-session.ts compaction methods).
///
/// Shares the `SessionManager` (`Arc<Mutex<_>>`) with the owning
/// `AgentSession`: the agent-event listener persists messages from the
/// prompt task while mode command tasks run session operations (T10
/// wiring). The agent is shared (`Arc<Agent>`) because the agent loop
/// drives it concurrently.
/// Shared handle to the in-flight compaction's cancellation token. Readable
/// without the runner's async mutex so `abortCompaction`/`dispose` still work
/// while a compaction is in flight (upstream aborts an AbortController
/// directly, agent-session.ts:1930-1933).
pub type AbortTokenCell = Arc<std::sync::Mutex<Option<CancellationToken>>>;

fn lock_abort(cell: &AbortTokenCell) -> std::sync::MutexGuard<'_, Option<CancellationToken>> {
    cell.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct CompactionRunner {
    agent: Arc<Agent>,
    session: Arc<Mutex<SessionManager>>,
    model: Option<Model>,
    settings: CompactionSettings,
    retry: Option<RetryPolicy>,
    stream_fn: StreamFn,
    thinking_level: ThinkingLevel,
    emit: CompactionEventSink,
    overflow_recovery_attempted: bool,
    /// Manual compaction's token — upstream `_compactionAbortController`
    /// (agent-session.ts:327). The prompt rejection reads only this cell
    /// (agent-session.ts:1133).
    active_token: AbortTokenCell,
    /// Auto compaction's token — upstream `_autoCompactionAbortController`
    /// (agent-session.ts:328). Kept separate from the manual cell so an
    /// in-flight auto compaction does not make `prompt` reject
    /// (agent-session.ts:1133 checks only the manual controller).
    auto_active_token: AbortTokenCell,
    /// Extension runner slot (T15 W2): read per emit so a runner swap
    /// (session replacement / reload) takes effect without rebuilding the
    /// runner. `None` in bare test fixtures = no extensions.
    extension_runner: Option<crate::core::extensions::ExtensionRunnerRef>,
}

impl CompactionRunner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent: Arc<Agent>,
        session: Arc<Mutex<SessionManager>>,
        model: Option<Model>,
        settings: CompactionSettings,
        retry: Option<RetryPolicy>,
        stream_fn: StreamFn,
        thinking_level: ThinkingLevel,
        emit: CompactionEventSink,
    ) -> Self {
        Self {
            agent,
            session,
            model,
            settings,
            retry,
            stream_fn,
            thinking_level,
            emit,
            overflow_recovery_attempted: false,
            active_token: AbortTokenCell::default(),
            auto_active_token: AbortTokenCell::default(),
            extension_runner: None,
        }
    }

    /// T15 W2: install the extension runner slot (AgentSession passes its
    /// shared `ExtensionRunnerRef`).
    pub fn set_extension_runner(
        &mut self,
        runner_ref: crate::core::extensions::ExtensionRunnerRef,
    ) {
        self.extension_runner = Some(runner_ref);
    }

    /// Current extension runner, if installed.
    fn extension_runner(&self) -> Option<Arc<dyn crate::core::extensions::ExtensionRunner>> {
        self.extension_runner
            .as_ref()
            .map(crate::core::extensions::read_runner)
    }

    /// Shared abort handle for the manual path (see [`AbortTokenCell`]).
    pub fn abort_token_cell(&self) -> AbortTokenCell {
        self.active_token.clone()
    }

    /// Shared abort handle for the auto-compaction path (upstream keeps a
    /// second controller, `_autoCompactionAbortController`,
    /// agent-session.ts:328). `abortCompaction` cancels both, but the prompt
    /// rejection must NOT consult this one (agent-session.ts:1133).
    pub fn auto_abort_token_cell(&self) -> AbortTokenCell {
        self.auto_active_token.clone()
    }

    /// Lock the shared session (poison-tolerant, like the rest of the
    /// codebase). Both accessors return the same guard; the two names mirror
    /// the pre-T10 `session()`/`session_mut()` pair.
    pub fn session(&self) -> MutexGuard<'_, SessionManager> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn session_mut(&self) -> MutexGuard<'_, SessionManager> {
        self.session()
    }

    /// `this.model` updates (setModel / cycleModel / restore) re-sync the
    /// runner with the session's current model.
    pub fn set_model(&mut self, model: Option<Model>) {
        self.model = model;
    }

    /// Re-read `settingsManager.getCompactionSettings()` before trigger
    /// checks (upstream reads settings per call).
    pub fn set_settings(&mut self, settings: CompactionSettings) {
        self.settings = settings;
    }

    pub fn set_retry(&mut self, retry: Option<RetryPolicy>) {
        self.retry = retry;
    }

    pub fn set_thinking_level(&mut self, thinking_level: ThinkingLevel) {
        self.thinking_level = thinking_level;
    }

    /// Re-point the event sink (T10: `AgentSession` forwards compaction
    /// events into its own event stream after construction resolves the
    /// circular reference).
    pub fn set_emit_sink(&mut self, sink: CompactionEventSink) {
        self.emit = sink;
    }

    /// Reset the one-shot overflow recovery budget — upstream resets it on
    /// new sessions and user prompts (agent-session.ts:599/651).
    pub fn reset_overflow_recovery(&mut self) {
        self.overflow_recovery_attempted = false;
    }

    /// `abortCompaction()` (agent-session.ts:1938-1941): cancels both the
    /// manual and the auto controller.
    pub fn abort_compaction(&self) {
        if let Some(token) = lock_abort(&self.active_token).as_ref() {
            token.cancel();
        }
        if let Some(token) = lock_abort(&self.auto_active_token).as_ref() {
            token.cancel();
        }
    }

    fn emit(&self, event: CompactionEvent) {
        (self.emit)(event);
    }

    /// `_summarizationRetryCallbacks` (agent-session.ts:2646-2669): forward
    /// retry progress as compaction events.
    fn summarization_retry_callbacks(&self, source: RetrySource) -> RetryCallbacks {
        let emit = self.emit.clone();
        let on_retry_scheduled =
            move |(attempt, max_attempts, delay_ms, error_message): (u32, u32, u64, String)| {
                let emit = emit.clone();
                Box::pin(async move {
                    emit(CompactionEvent::SummarizationRetryScheduled {
                        attempt,
                        max_attempts,
                        delay_ms,
                        error_message,
                    });
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            };
        let emit = self.emit.clone();
        let on_retry_attempt_start = move |(): ()| {
            let emit = emit.clone();
            Box::pin(async move {
                emit(CompactionEvent::SummarizationRetryAttemptStart { source });
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        };
        let emit = self.emit.clone();
        let on_retry_finished = move |_: (bool, u32, Option<String>)| {
            let emit = emit.clone();
            Box::pin(async move {
                emit(CompactionEvent::SummarizationRetryFinished);
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        };
        RetryCallbacks {
            on_retry_scheduled: Some(Box::new(on_retry_scheduled)),
            on_retry_attempt_start: Some(Box::new(on_retry_attempt_start)),
            on_retry_finished: Some(Box::new(on_retry_finished)),
        }
    }

    /// Typed branch path. Raw/unknown entries are dropped: they produce no
    /// context messages, so the cut-point and preparation logic sees the same
    /// stream upstream sees for entries it knows.
    fn path_entries(&self) -> Vec<SessionEntry> {
        self.session()
            .get_branch(None)
            .iter()
            .filter_map(|entry| entry.known().cloned())
            .collect()
    }

    /// `compact()` (agent-session.ts:1783-1925): manual compaction API.
    pub async fn compact(
        &mut self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, RpiError> {
        let token = CancellationToken::new();
        *lock_abort(&self.active_token) = Some(token.clone());
        self.emit(CompactionEvent::CompactionStart {
            reason: CompactionReason::Manual,
        });

        let outcome = self.compact_inner(custom_instructions, &token).await;

        // Upstream: aborted = message === "Compaction cancelled" ||
        // error.name === "AbortError" (agent-session.ts:1911). Sources here:
        // the abort token, and an extension's `session_before_compact`
        // cancel (:1819-1821).
        let aborted = match &outcome {
            Err(error) => {
                token.is_cancelled() || raw_error_message(error) == "Compaction cancelled"
            }
            Ok(_) => false,
        };
        let (result, error_message) = match &outcome {
            Ok(result) => (Some(Box::new(result.clone())), None),
            Err(error) if aborted => (None, None),
            Err(error) => (
                None,
                Some(format!("Compaction failed: {}", raw_error_message(error))),
            ),
        };
        // compaction_end listeners may submit queued prompts, so expose idle
        // state before notifying them (agent-session.ts:1907-1908 @ 3852cb2b8).
        *lock_abort(&self.active_token) = None;
        self.emit(CompactionEvent::CompactionEnd {
            reason: CompactionReason::Manual,
            result,
            aborted,
            will_retry: false,
            error_message,
        });
        outcome
    }

    /// `session_before_compact` emit shared by the manual and auto paths
    /// (agent-session.ts:1812-1831 / :2079-2105). `Ok(None)` proceeds with
    /// the default summarization; `Ok(Some(_))` is an extension-provided
    /// compaction (`fromExtension`); `Err("Compaction cancelled")` cancels.
    async fn emit_session_before_compact(
        &self,
        preparation: &rpi_agent::compaction::CompactionPreparation,
        path_entries: &[SessionEntry],
        custom_instructions: Option<&str>,
        reason: CompactionReason,
        will_retry: bool,
    ) -> Result<Option<CompactionResult>, RpiError> {
        let Some(runner) = self.extension_runner() else {
            return Ok(None);
        };
        if !runner.has_handlers("session_before_compact") {
            return Ok(None);
        }
        let event = crate::core::extensions::SessionBeforeCompactEvent {
            preparation: serde_json::to_value(preparation)
                .map_err(|error| RpiError::Session(error.to_string()))?,
            branch_entries: serde_json::to_value(path_entries)
                .map_err(|error| RpiError::Session(error.to_string()))?,
            custom_instructions: custom_instructions.map(str::to_owned),
            reason: reason.as_str().to_owned(),
            will_retry,
        };
        let Some(result) = runner.emit_session_before_compact(&event).await else {
            return Ok(None);
        };
        if result.cancel == Some(true) {
            return Err(RpiError::Session("Compaction cancelled".into()));
        }
        Ok(result.compaction)
    }

    async fn compact_inner(
        &mut self,
        custom_instructions: Option<&str>,
        token: &CancellationToken,
    ) -> Result<CompactionResult, RpiError> {
        // `if (!this.model) throw new Error(formatNoModelSelectedMessage())`
        // (agent-session.ts:1790-1792).
        let Some(model) = self.model.clone() else {
            return Err(RpiError::Session(format_no_model_selected_message()));
        };
        let path_entries = self.path_entries();
        let preparation = prepare_compaction(&path_entries, &self.settings).ok_or_else(|| {
            // Check why we can't compact (agent-session.ts:1801-1807).
            match path_entries.last() {
                Some(SessionEntry::Compaction(_)) => RpiError::Session("Already compacted".into()),
                _ => RpiError::Session("Nothing to compact (session too small)".into()),
            }
        })?;

        // Extension-provided compaction (agent-session.ts:1809-1828).
        let extension_compaction = self
            .emit_session_before_compact(
                &preparation,
                &path_entries,
                custom_instructions,
                CompactionReason::Manual,
                false,
            )
            .await?;
        let from_extension = extension_compaction.is_some();

        let result = match extension_compaction {
            Some(compaction) => compaction,
            None => {
                let args = SummarizationArgs {
                    signal: Some(token.clone()),
                    thinking_level: Some(self.thinking_level),
                    retry: self.retry,
                    ..Default::default()
                };
                let callbacks = self.summarization_retry_callbacks(RetrySource::Compaction {
                    reason: CompactionReason::Manual,
                });
                run_compact(
                    &preparation,
                    &model,
                    custom_instructions,
                    &self.stream_fn,
                    &args,
                    Some(&callbacks),
                )
                .await
                .map_err(|error| RpiError::Session(error.to_string()))?
            }
        };

        if token.is_cancelled() {
            return Err(RpiError::Session("Compaction cancelled".into()));
        }

        self.finish_compaction(result, from_extension, CompactionReason::Manual, false)
            .await
    }

    /// Append the compaction entry, rebuild context, emit `session_compact`,
    /// and compute the result payload shared by the manual and auto paths
    /// (agent-session.ts:1872-1900 / :2153-2181).
    async fn finish_compaction(
        &mut self,
        result: CompactionResult,
        from_extension: bool,
        reason: CompactionReason,
        will_retry: bool,
    ) -> Result<CompactionResult, RpiError> {
        self.session_mut().append_compaction(
            &result.summary,
            &result.first_kept_entry_id,
            result.tokens_before,
            result.details.clone(),
            Some(from_extension),
            result.usage.clone(),
        )?;
        let session_context = self.session().build_session_context();
        self.agent.set_messages(session_context.messages.clone());
        let estimated_tokens_after = estimate_messages_tokens(&session_context.messages);

        // `session_compact` with the saved entry (found by summary,
        // agent-session.ts:1876-1891 / :2156-2172).
        if let Some(runner) = self.extension_runner() {
            if runner.has_handlers("session_compact") {
                let saved_entry = self
                    .session()
                    .get_entries()
                    .into_iter()
                    .filter_map(|entry| entry.known().cloned())
                    .find(
                        |entry| matches!(entry, SessionEntry::Compaction(compaction) if compaction.summary == result.summary),
                    );
                if let Some(entry) = saved_entry {
                    match serde_json::to_value(&entry) {
                        Ok(value) => {
                            runner
                                .emit_session_compact(
                                    value,
                                    from_extension,
                                    reason.as_str(),
                                    will_retry,
                                )
                                .await;
                        }
                        Err(error) => {
                            tracing::warn!("session_compact payload serialize failed: {error}")
                        }
                    }
                }
            }
        }

        Ok(CompactionResult {
            estimated_tokens_after: Some(estimated_tokens_after),
            ..result
        })
    }

    /// `_checkCompaction` (agent-session.ts:1953-2042).
    ///
    /// Called after `agent_end` (`skip_aborted_check = true`) and before
    /// prompt submission (`false`). Returns whether the caller should
    /// `agent.continue` (overflow retry or queued-message drain).
    pub async fn check_compaction(
        &mut self,
        assistant_message: &AssistantMessage,
        skip_aborted_check: bool,
    ) -> bool {
        if !self.settings.enabled {
            return false;
        }

        // Skip if message was aborted (user cancelled) — unless
        // skipAbortedCheck is false.
        if skip_aborted_check && assistant_message.stop_reason == StopReason::Aborted {
            return false;
        }

        // `this.model?.contextWindow ?? 0` (agent-session.ts:1960).
        let context_window = self
            .model
            .as_ref()
            .map(|model| u64::from(model.context_window))
            .unwrap_or(0);

        // Skip the overflow check if the message came from a different model
        // (agent-session.ts:1962-1967). `this.model && ...` — falsy without a
        // model.
        let same_model = self.model.as_ref().is_some_and(|model| {
            assistant_message.provider == model.provider && assistant_message.model == model.id
        });

        // Skip compaction checks if this assistant message is older than the
        // latest compaction boundary (agent-session.ts:1969-1977).
        let path = self.path_entries();
        let compaction_entry = get_latest_compaction_entry(&path);
        let compaction_ts = compaction_entry.and_then(|entry| parse_iso8601_ms(&entry.timestamp));
        if let Some(ts) = compaction_ts {
            if assistant_message.timestamp <= ts {
                return false;
            }
        }

        // Case 1: Recoverable failure (agent-session.ts:1990-2001 @ 32850ef7c).
        // Explicit/silent context overflow still uses context metadata.
        // A length stop is recoverable when output ended below the model's
        // original desired limit, independent of the configured context size
        // or any context-clamped provider request limit. A successful response
        // over the configured window should compact but must not retry: the
        // assistant answer already completed and agent.continue() cannot
        // continue from an assistant.
        let max_tokens = self
            .model
            .as_ref()
            .map(|m| u64::from(m.max_tokens))
            .unwrap_or(0);
        let recoverable_length = same_model && is_recoverable_length(assistant_message, max_tokens);
        if same_model
            && (is_context_overflow(assistant_message, Some(context_window)) || recoverable_length)
        {
            let will_retry = assistant_message.stop_reason != StopReason::Stop;

            if !will_retry {
                return self
                    .run_auto_compaction(CompactionReason::Overflow, false)
                    .await;
            }

            if self.overflow_recovery_attempted {
                self.emit(CompactionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(
                        "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                            .to_owned(),
                    ),
                });
                return false;
            }

            self.overflow_recovery_attempted = true;
            // Remove the error message from agent state (it IS saved to the
            // session for history, but we don't want it in context for the
            // retry).
            let mut messages = self.agent.state().messages;
            if matches!(messages.last(), Some(rpi_agent::AgentMessage::Assistant(_))) {
                messages.pop();
                self.agent.set_messages(messages);
            }
            return self
                .run_auto_compaction(CompactionReason::Overflow, will_retry)
                .await;
        }

        // Case 2: threshold (agent-session.ts:2013-2041). For error messages
        // or all-zero usage, estimate from the last valid response.
        let direct_context_tokens = calculate_context_tokens(&assistant_message.usage);
        let context_tokens =
            if assistant_message.stop_reason == StopReason::Error || direct_context_tokens == 0 {
                let messages = self.agent.state().messages;
                let estimate = estimate_context_tokens(&messages);
                let Some(last_usage_index) = estimate.last_usage_index else {
                    return false; // No usage data at all.
                };
                // Verify the usage source is post-compaction (stale usage guard,
                // agent-session.ts:2023-2033).
                if let (Some(ts), rpi_agent::AgentMessage::Assistant(usage_msg)) =
                    (compaction_ts, &messages[last_usage_index])
                {
                    if usage_msg.timestamp <= ts {
                        return false;
                    }
                }
                estimate.tokens
            } else {
                direct_context_tokens
            };
        if should_compact(context_tokens, context_window, &self.settings) {
            return self
                .run_auto_compaction(CompactionReason::Threshold, false)
                .await;
        }
        false
    }

    /// `_runAutoCompaction` (agent-session.ts:2047-2215). Never fails
    /// outward: errors are reported via `compaction_end` and yield `false`.
    async fn run_auto_compaction(&mut self, reason: CompactionReason, will_retry: bool) -> bool {
        let mut started = false;
        let token = CancellationToken::new();

        let outcome: Result<bool, RpiError> = async {
            // `if (!this.model) return false` (agent-session.ts:2052-2054).
            let Some(model) = self.model.clone() else {
                return Ok(false);
            };
            let path_entries = self.path_entries();
            let Some(preparation) = prepare_compaction(&path_entries, &self.settings) else {
                return Ok(false);
            };

            self.emit(CompactionEvent::CompactionStart { reason });
            *lock_abort(&self.auto_active_token) = Some(token.clone());
            started = true;

            // Extension-provided compaction (agent-session.ts:2079-2105).
            let extension_compaction = match self
                .emit_session_before_compact(
                    &preparation,
                    &path_entries,
                    None,
                    reason,
                    will_retry,
                )
                .await
            {
                Ok(compaction) => compaction,
                Err(error) if raw_error_message(&error) == "Compaction cancelled" => {
                    // Extension cancel (agent-session.ts:2085-2092).
                    // Clear idle state before compaction_end so listeners can
                    // submit queued prompts (agent-session.ts:1907-1908 @ 3852cb2b8).
                    *lock_abort(&self.auto_active_token) = None;
                    self.emit(CompactionEvent::CompactionEnd {
                        reason,
                        result: None,
                        aborted: true,
                        will_retry: false,
                        error_message: None,
                    });
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            let from_extension = extension_compaction.is_some();

            let result = match extension_compaction {
                Some(compaction) => compaction,
                None => {
                    let args = SummarizationArgs {
                        signal: Some(token.clone()),
                        thinking_level: Some(self.thinking_level),
                        retry: self.retry,
                        ..Default::default()
                    };
                    let callbacks =
                        self.summarization_retry_callbacks(RetrySource::Compaction { reason });
                    run_compact(&preparation, &model, None, &self.stream_fn, &args, Some(&callbacks))
                        .await
                        .map_err(|error| RpiError::Session(error.to_string()))?
                }
            };

            if token.is_cancelled() {
                // Clear idle state before compaction_end (3852cb2b8).
                *lock_abort(&self.auto_active_token) = None;
                self.emit(CompactionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: None,
                });
                return Ok(false);
            }

            let result = self
                .finish_compaction(result, from_extension, reason, will_retry)
                .await?;
            // Clear idle state before compaction_end so listeners can submit
            // queued prompts (agent-session.ts:1907-1908 @ 3852cb2b8).
            *lock_abort(&self.auto_active_token) = None;
            self.emit(CompactionEvent::CompactionEnd {
                reason,
                result: Some(Box::new(result)),
                aborted: false,
                will_retry,
                error_message: None,
            });

            if will_retry {
                let mut messages = self.agent.state().messages;
                // The overflow response was persisted on message_end before
                // _checkCompaction() removed it from agent state. Rebuilding
                // state from the new compaction can restore that kept entry,
                // leaving an assistant as the final message. agent.continue()
                // rejects that state, so remove the retriable error or
                // truncated-length response again before continuing the
                // interrupted turn (agent-session.ts:2184-2189 @ 32850ef7c).
                if matches!(
                    messages.last(),
                    Some(rpi_agent::AgentMessage::Assistant(m)) if m.stop_reason == StopReason::Error || m.stop_reason == StopReason::Length
                ) {
                    messages.pop();
                    self.agent.set_messages(messages);
                }
                return Ok(true);
            }

            // Auto-compaction can complete while follow-up/steering/custom
            // messages are waiting. Continue once so queued messages are
            // delivered.
            Ok(self.agent.has_queued_messages())
        }
        .await;

        *lock_abort(&self.auto_active_token) = None;

        match outcome {
            Ok(should_continue) => should_continue,
            Err(error) => {
                if started {
                    let message = match reason {
                        CompactionReason::Overflow => {
                            format!(
                                "Context overflow recovery failed: {}",
                                raw_error_message(&error)
                            )
                        }
                        _ => format!("Auto-compaction failed: {}", raw_error_message(&error)),
                    };
                    self.emit(CompactionEvent::CompactionEnd {
                        reason,
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(message),
                    });
                }
                false
            }
        }
    }
}
