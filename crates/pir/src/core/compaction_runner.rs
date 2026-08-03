//! Port of the compaction wiring of
//! `packages/coding-agent/src/core/agent-session.ts` @ pi 0.82.1 (2efa728):
//! `compact()` (:1783-1925), `_checkCompaction` (:1953-2042),
//! `_runAutoCompaction` (:2047-2215), and `_summarizationRetryCallbacks`
//! (:2646-2669).
//!
//! Owns the two trigger paths (post-`agent_end` and pre-prompt share
//! [`CompactionRunner::check_compaction`]; the caller decides whether
//! `agent.continue` follows), overflow recovery, and the manual compaction
//! API. The pure compaction logic lives in `pir_agent::compaction`; session
//! I/O goes through [`SessionManager`].
//!
//! Intentional differences:
//! - Extension hooks (`session_before_compact` / `session_compact`) are not
//!   ported — the extension host lands later (T17+). `fromExtension` is
//!   therefore always false.
//! - Unknown/raw session entries are dropped when converting the branch to
//!   typed entries; they carry zero context, so `prepareCompaction` sees the
//!   same message stream either way.
//! - `this.model` is guaranteed by the caller, so the runner holds a
//!   [`Model`] directly; the `formatNoModelSelectedMessage()` branch is
//!   unreachable here.
//! - Auth (`_getSummarizationRequestAuth`) is out of scope for the faux-first
//!   path: `apiKey`/`headers`/`env` ride [`SummarizationArgs`] and default to
//!   `None`, exactly what a custom `streamFn` receives upstream.

use std::sync::Arc;

use pir_agent::compaction::{
    calculate_context_tokens, compact as run_compact, estimate_context_tokens,
    estimate_messages_tokens, prepare_compaction, should_compact, CompactionResult,
    CompactionSettings, SummarizationArgs,
};
use pir_agent::session::{get_latest_compaction_entry, parse_iso8601_ms, SessionEntry};
use pir_agent::types::ThinkingLevel;
use pir_agent::{Agent, StreamFn};
use pir_ai::types::{AssistantMessage, Model, StopReason};
use pir_ai::utils::overflow::is_context_overflow;
use pir_ai::utils::retry::{RetryCallbacks, RetryPolicy};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::core::session_manager::SessionManager;
use crate::error::PirError;

/// `"manual" | "threshold" | "overflow"` (agent-session.ts:152).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
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
/// without the `PirError` Display prefix.
fn raw_error_message(error: &PirError) -> String {
    match error {
        PirError::Session(message) => message.clone(),
        other => other.to_string(),
    }
}

/// Compaction trigger wiring (agent-session.ts compaction methods).
///
/// Owns the `SessionManager` like upstream's `AgentSession` does; the agent
/// is shared (`Arc<Agent>`) because the agent loop drives it concurrently.
pub struct CompactionRunner {
    agent: Arc<Agent>,
    session: SessionManager,
    model: Model,
    settings: CompactionSettings,
    retry: Option<RetryPolicy>,
    stream_fn: StreamFn,
    thinking_level: ThinkingLevel,
    emit: CompactionEventSink,
    overflow_recovery_attempted: bool,
    active_token: Option<CancellationToken>,
}

impl CompactionRunner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent: Arc<Agent>,
        session: SessionManager,
        model: Model,
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
            active_token: None,
        }
    }

    pub fn session(&self) -> &SessionManager {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut SessionManager {
        &mut self.session
    }

    pub fn into_session(self) -> SessionManager {
        self.session
    }

    /// Reset the one-shot overflow recovery budget — upstream resets it on
    /// new sessions and user prompts (agent-session.ts:599/651).
    pub fn reset_overflow_recovery(&mut self) {
        self.overflow_recovery_attempted = false;
    }

    /// `abortCompaction()` (agent-session.ts:1930-1933).
    pub fn abort_compaction(&self) {
        if let Some(token) = &self.active_token {
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
        self.session
            .get_branch(None)
            .iter()
            .filter_map(|entry| entry.known().cloned())
            .collect()
    }

    /// `compact()` (agent-session.ts:1783-1925): manual compaction API.
    pub async fn compact(
        &mut self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, PirError> {
        let token = CancellationToken::new();
        self.active_token = Some(token.clone());
        self.emit(CompactionEvent::CompactionStart {
            reason: CompactionReason::Manual,
        });

        let outcome = self.compact_inner(custom_instructions, &token).await;

        // Upstream: aborted = message === "Compaction cancelled" ||
        // error.name === "AbortError" (agent-session.ts:1911). The only
        // "Compaction cancelled" source here is the token check.
        let aborted = outcome.is_err() && token.is_cancelled();
        let (result, error_message) = match &outcome {
            Ok(result) => (Some(Box::new(result.clone())), None),
            Err(error) if aborted => (None, None),
            Err(error) => (
                None,
                Some(format!("Compaction failed: {}", raw_error_message(error))),
            ),
        };
        self.emit(CompactionEvent::CompactionEnd {
            reason: CompactionReason::Manual,
            result,
            aborted,
            will_retry: false,
            error_message,
        });
        self.active_token = None;
        outcome
    }

    async fn compact_inner(
        &mut self,
        custom_instructions: Option<&str>,
        token: &CancellationToken,
    ) -> Result<CompactionResult, PirError> {
        let path_entries = self.path_entries();
        let preparation = prepare_compaction(&path_entries, &self.settings).ok_or_else(|| {
            // Check why we can't compact (agent-session.ts:1801-1807).
            match path_entries.last() {
                Some(SessionEntry::Compaction(_)) => PirError::Session("Already compacted".into()),
                _ => PirError::Session("Nothing to compact (session too small)".into()),
            }
        })?;

        let args = SummarizationArgs {
            signal: Some(token.clone()),
            thinking_level: Some(self.thinking_level),
            retry: self.retry,
            ..Default::default()
        };
        let callbacks = self.summarization_retry_callbacks(RetrySource::Compaction {
            reason: CompactionReason::Manual,
        });
        let result = run_compact(
            &preparation,
            &self.model,
            custom_instructions,
            &self.stream_fn,
            &args,
            Some(&callbacks),
        )
        .await
        .map_err(|error| PirError::Session(error.to_string()))?;

        if token.is_cancelled() {
            return Err(PirError::Session("Compaction cancelled".into()));
        }

        self.finish_compaction(result)
    }

    /// Append the compaction entry, rebuild context, and compute the result
    /// payload shared by the manual and auto paths
    /// (agent-session.ts:1872-1900 / :2153-2181).
    fn finish_compaction(
        &mut self,
        result: CompactionResult,
    ) -> Result<CompactionResult, PirError> {
        self.session.append_compaction(
            &result.summary,
            &result.first_kept_entry_id,
            result.tokens_before,
            result.details.clone(),
            Some(false),
            result.usage.clone(),
        )?;
        let session_context = self.session.build_session_context();
        self.agent.set_messages(session_context.messages.clone());
        let estimated_tokens_after = estimate_messages_tokens(&session_context.messages);

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

        let context_window = u64::from(self.model.context_window);

        // Skip the overflow check if the message came from a different model
        // (agent-session.ts:1962-1967).
        let same_model = assistant_message.provider == self.model.provider
            && assistant_message.model == self.model.id;

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

        // Case 1: overflow (agent-session.ts:1979-2011).
        if same_model && is_context_overflow(assistant_message, Some(context_window)) {
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
            if matches!(messages.last(), Some(pir_agent::AgentMessage::Assistant(_))) {
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
                if let (Some(ts), pir_agent::AgentMessage::Assistant(usage_msg)) =
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

        let outcome: Result<bool, PirError> = async {
            let path_entries = self.path_entries();
            let Some(preparation) = prepare_compaction(&path_entries, &self.settings) else {
                return Ok(false);
            };

            self.emit(CompactionEvent::CompactionStart { reason });
            self.active_token = Some(token.clone());
            started = true;

            let args = SummarizationArgs {
                signal: Some(token.clone()),
                thinking_level: Some(self.thinking_level),
                retry: self.retry,
                ..Default::default()
            };
            let callbacks = self.summarization_retry_callbacks(RetrySource::Compaction { reason });
            let result = run_compact(&preparation, &self.model, None, &self.stream_fn, &args, Some(&callbacks))
                .await
                .map_err(|error| PirError::Session(error.to_string()))?;

            if token.is_cancelled() {
                self.emit(CompactionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: None,
                });
                return Ok(false);
            }

            let result = self.finish_compaction(result)?;
            self.emit(CompactionEvent::CompactionEnd {
                reason,
                result: Some(Box::new(result)),
                aborted: false,
                will_retry,
                error_message: None,
            });

            if will_retry {
                let mut messages = self.agent.state().messages;
                if matches!(
                    messages.last(),
                    Some(pir_agent::AgentMessage::Assistant(m)) if m.stop_reason == StopReason::Error
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

        self.active_token = None;

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
