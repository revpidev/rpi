//! Branch coverage of `CompactionRunner::check_compaction` (every guard of
//! agent-session.ts `_checkCompaction` :1953-2042) plus the manual
//! `compact()` error paths (:1799-1807/:1868-1870). The happy path is
//! covered by the `parity_compaction_test.rs` parity suite.

use std::sync::{Arc, Mutex};

use rpi::core::compaction_runner::{CompactionEvent, CompactionRunner};
use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi_agent::compaction::CompactionSettings;
use rpi_agent::messages::AgentMessage;
use rpi_agent::types::ThinkingLevel;
use rpi_agent::{Agent, AgentOptions, InitialAgentState};
use rpi_ai::types::{AssistantMessage, StopReason, Usage};
use rpi_test_support::faux::{
    faux_assistant_message, FauxAssistantOptions, FauxModelDefinition, FauxProvider,
    FauxProviderOptions, FauxResponseStep,
};
use serde_json::json;

fn usage(total: u64) -> Usage {
    serde_json::from_value(json!({
        "input": total,
        "output": 0,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": total,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    }))
    .expect("usage")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn assistant(
    text: &str,
    stop_reason: StopReason,
    total_tokens: u64,
    timestamp: i64,
) -> AssistantMessage {
    let mut message = faux_assistant_message(
        text,
        FauxAssistantOptions {
            stop_reason: Some(stop_reason),
            ..Default::default()
        },
    );
    message.usage = usage(total_tokens);
    message.timestamp = timestamp;
    message
}

fn overflow_error() -> AssistantMessage {
    let mut message = faux_assistant_message(
        "",
        FauxAssistantOptions {
            stop_reason: Some(StopReason::Error),
            error_message: Some("prompt is too long: 99999 tokens > 8192 maximum".to_owned()),
            ..Default::default()
        },
    );
    message.timestamp = now_ms();
    message
}

fn user(text: &str) -> AgentMessage {
    serde_json::from_value(json!({
        "role": "user",
        "content": text,
        "timestamp": 1,
    }))
    .expect("user message")
}

struct Fixture {
    agent: Arc<Agent>,
    runner: CompactionRunner,
    events: Arc<Mutex<Vec<CompactionEvent>>>,
}

/// faux model with window 8192 + in-memory session; `responses` feeds the compaction
/// summary calls.
fn fixture(settings: CompactionSettings, responses: Vec<FauxResponseStep>) -> Fixture {
    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(8192),
            max_tokens: Some(65536),
        }]),
        ..Default::default()
    });
    provider.set_responses(responses);
    let model = provider.get_model(None).expect("faux-1");

    let mut options = AgentOptions::new(provider.stream_fn());
    options.initial_state = InitialAgentState {
        model: Some(model.clone()),
        thinking_level: Some(ThinkingLevel::Off),
        ..Default::default()
    };
    let agent = Arc::new(Agent::new(options));

    let session = SessionManager::in_memory(None, NewSessionOptions::default()).expect("session");
    let events: Arc<Mutex<Vec<CompactionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let runner = CompactionRunner::new(
        agent.clone(),
        Arc::new(Mutex::new(session)),
        Some(model),
        settings,
        None,
        provider.stream_fn(),
        ThinkingLevel::Off,
        Arc::new(move |event| sink.lock().expect("events").push(event)),
    );
    Fixture {
        agent,
        runner,
        events,
    }
}

fn settings() -> CompactionSettings {
    CompactionSettings {
        enabled: true,
        reserve_tokens: 4096,
        keep_recent_tokens: 10,
    }
}

fn scripted(text: &str) -> FauxResponseStep {
    let text = text.to_owned();
    FauxResponseStep::Factory(Box::new(move |_context, _options, _state, _model| {
        let mut message = faux_assistant_message(text.clone(), FauxAssistantOptions::default());
        message.timestamp = now_ms();
        message
    }))
}

impl Fixture {
    /// Lays one conversation round into the session and syncs agent state.
    fn seed_turn(&mut self, user_text: &str, assistant_text: &str, total_tokens: u64) {
        let user = user(user_text);
        let assistant = assistant(assistant_text, StopReason::Stop, total_tokens, now_ms());
        self.runner
            .session_mut()
            .append_message(user.clone())
            .expect("append user");
        self.runner
            .session_mut()
            .append_message(AgentMessage::Assistant(assistant.clone()))
            .expect("append assistant");
        let mut messages = self.agent.state().messages;
        messages.push(user);
        messages.push(AgentMessage::Assistant(assistant));
        self.agent.set_messages(messages);
    }

    fn events(&self) -> Vec<CompactionEvent> {
        self.events.lock().expect("events").clone()
    }
}

fn compaction_end_errors(events: &[CompactionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            CompactionEvent::CompactionEnd {
                error_message: Some(message),
                ..
            } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn event_types(events: &[CompactionEvent]) -> Vec<&str> {
    events
        .iter()
        .map(|event| match event {
            CompactionEvent::CompactionStart { .. } => "compaction_start",
            CompactionEvent::CompactionEnd { .. } => "compaction_end",
            CompactionEvent::SummarizationRetryScheduled { .. } => "summarization_retry_scheduled",
            CompactionEvent::SummarizationRetryAttemptStart { .. } => {
                "summarization_retry_attempt_start"
            }
            CompactionEvent::SummarizationRetryFinished => "summarization_retry_finished",
        })
        .collect()
}

// ---------------------------------------------------------------------------
// _checkCompaction guards
// ---------------------------------------------------------------------------

/// `settings.enabled = false` short-circuits every check (agent-session.ts:1955).
#[tokio::test]
async fn check_compaction_disabled_settings_short_circuits() {
    let mut fixture = fixture(
        CompactionSettings {
            enabled: false,
            ..settings()
        },
        Vec::new(),
    );
    fixture.seed_turn("q", "a", 100);
    assert!(
        !fixture
            .runner
            .check_compaction(&overflow_error(), true)
            .await
    );
    assert!(fixture.events().is_empty());
}

/// `skipAbortedCheck = true` skips aborted messages (:1958); the pre-prompt path
/// (false) does not skip — aborted messages take the threshold-estimate branch.
#[tokio::test]
async fn check_compaction_aborted_message_skipped_only_for_post_run() {
    let mut fixture = fixture(settings(), Vec::new());
    fixture.seed_turn("q", "a", 100);
    let aborted = assistant("partial", StopReason::Aborted, 100, now_ms());
    assert!(!fixture.runner.check_compaction(&aborted, true).await);
    assert!(fixture.events().is_empty());
}

/// Same-model guard: overflow errors from other models do not trigger (:1966-1967).
#[tokio::test]
async fn check_compaction_overflow_from_other_model_is_ignored() {
    let mut fixture = fixture(settings(), Vec::new());
    fixture.seed_turn("q", "a", 100);
    let mut foreign = overflow_error();
    foreign.provider = "other-provider".to_owned();
    assert!(!fixture.runner.check_compaction(&foreign, true).await);
    let mut foreign_id = overflow_error();
    foreign_id.model = "other-model".to_owned();
    assert!(!fixture.runner.check_compaction(&foreign_id, true).await);
    assert!(fixture.events().is_empty());
}

/// Stale guard: an assistant timestamp older than the newest compaction entry is
/// skipped (:1972-1977).
#[tokio::test]
async fn check_compaction_stale_message_before_compaction_boundary_is_ignored() {
    let mut fixture = fixture(settings(), Vec::new());
    fixture.seed_turn("q", "a", 100);
    fixture
        .runner
        .session_mut()
        .append_compaction("summary", "nonexistent-kept-id", 10, None, None, None)
        .expect("append compaction");
    // Timestamp 1 is necessarily older than the just-written compaction entry.
    let stale = assistant("old", StopReason::Stop, 99999, 1);
    assert!(!fixture.runner.check_compaction(&stale, true).await);
    assert!(fixture.events().is_empty());
}

/// Overflow with stop (silent overrun, usage.input+cacheRead > window) compacts but
/// does not retry:
/// willRetry=false（:1984-1987）。
#[tokio::test]
async fn check_compaction_silent_overflow_compacts_without_retry() {
    let mut fixture = fixture(settings(), vec![scripted("SUMMARY")]);
    fixture.seed_turn(
        &format!("q {}", "x".repeat(80)),
        &format!("a {}", "y".repeat(80)),
        100,
    );

    // stopReason "stop" and input > contextWindow(8192) → overflow Case 2.
    let silent_overflow = assistant("big answer", StopReason::Stop, 9000, now_ms());
    // After the threshold check, hasQueuedMessages() = false.
    assert!(
        !fixture
            .runner
            .check_compaction(&silent_overflow, true)
            .await
    );
    let events = fixture.events();
    assert_eq!(
        event_types(&events),
        vec!["compaction_start", "compaction_end"]
    );
    match &events[1] {
        CompactionEvent::CompactionEnd {
            reason,
            will_retry,
            aborted,
            result,
            ..
        } => {
            assert_eq!(
                *reason,
                rpi::core::compaction_runner::CompactionReason::Overflow
            );
            assert!(!will_retry);
            assert!(!aborted);
            assert!(result.is_some());
        }
        other => panic!("expected compaction_end, got {other:?}"),
    }
}

/// One-shot overflow recovery: after the first recovery, another overflow emits a
/// recovery-failure event and returns
/// false（:1990-2001）。
#[tokio::test]
async fn check_compaction_overflow_recovery_is_attempted_only_once() {
    let mut fixture = fixture(settings(), vec![scripted("SUMMARY")]);
    fixture.seed_turn(
        &format!("q {}", "x".repeat(80)),
        &format!("a {}", "y".repeat(80)),
        100,
    );
    // Agent state ends with the error message (the recovery path pops it first).
    let mut messages = fixture.agent.state().messages;
    messages.push(AgentMessage::Assistant(overflow_error()));
    fixture.agent.set_messages(messages);

    // First: recovery compaction + willRetry → true.
    assert!(
        fixture
            .runner
            .check_compaction(&overflow_error(), true)
            .await
    );
    // Second (no successful reply in between to reset the budget): recovery-failure
    // event. The timestamp must be strictly later than the compaction entry written by
    // the first recovery (the same millisecond would be intercepted by the :366 stale
    // guard); an explicit +1s removes the timing race.
    let mut second = overflow_error();
    second.timestamp = now_ms() + 1000;
    assert!(!fixture.runner.check_compaction(&second, true).await);
    let errors = compaction_end_errors(&fixture.events());
    assert_eq!(
        errors,
        vec![
            "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                .to_owned()
        ]
    );
}

/// Threshold branch: error/zero-usage messages estimate with the last valid usage
/// (:2017-2037).
#[tokio::test]
async fn check_compaction_threshold_estimates_from_last_valid_usage() {
    let mut fixture = fixture(settings(), vec![scripted("SUMMARY")]);
    fixture.seed_turn(
        &format!("q {}", "x".repeat(80)),
        &format!("a {}", "y".repeat(80)),
        5000,
    );

    // A non-overflow ordinary error + zero usage → estimate path; anchor 5000 > 8192-4096.
    let mut error = assistant("", StopReason::Error, 0, now_ms());
    error.error_message = Some("boom".to_owned());
    assert!(!fixture.runner.check_compaction(&error, true).await); // no retry
    let events = fixture.events();
    assert_eq!(
        event_types(&events),
        vec!["compaction_start", "compaction_end"]
    );
    match &events[0] {
        CompactionEvent::CompactionStart { reason } => {
            assert_eq!(
                *reason,
                rpi::core::compaction_runner::CompactionReason::Threshold
            );
        }
        other => panic!("expected compaction_start, got {other:?}"),
    }
}

/// The estimate path finds no valid usage at all → false (:2022).
#[tokio::test]
async fn check_compaction_threshold_without_any_usage_data_is_ignored() {
    let mut fixture = fixture(settings(), Vec::new());
    // Agent state has only user messages; no assistant usage at all.
    fixture.agent.set_messages(vec![user("q")]);
    let mut error = assistant("", StopReason::Error, 0, now_ms());
    error.error_message = Some("boom".to_owned());
    assert!(!fixture.runner.check_compaction(&error, true).await);
    assert!(fixture.events().is_empty());
}

/// Usage timestamp guard: an anchor usage from before the compaction is → false
/// (:2026-2033).
#[tokio::test]
async fn check_compaction_threshold_stale_usage_anchor_is_ignored() {
    let mut fixture = fixture(settings(), Vec::new());
    // Agent state: one high-usage assistant with timestamp 1; the session holds a
    // newer compaction entry.
    fixture.agent.set_messages(vec![
        user("q"),
        AgentMessage::Assistant(assistant("a", StopReason::Stop, 99999, 1)),
    ]);
    fixture
        .runner
        .session_mut()
        .append_compaction("summary", "nonexistent-kept-id", 10, None, None, None)
        .expect("append compaction");
    let mut error = assistant("", StopReason::Error, 0, now_ms());
    error.error_message = Some("boom".to_owned());
    assert!(!fixture.runner.check_compaction(&error, true).await);
    assert!(fixture.events().is_empty());
}

// ---------------------------------------------------------------------------
// Manual compact()
// ---------------------------------------------------------------------------

/// Manual compact: session too small → "Nothing to compact"; after success, compacting
/// again →
/// "Already compacted"（:1799-1807）。
#[tokio::test]
async fn manual_compact_error_paths_and_success() {
    let mut fixture = fixture(settings(), vec![scripted("MANUAL SUMMARY")]);

    // Empty session: nothing to compact.
    let error = fixture.runner.compact(None).await.expect_err("too small");
    assert_eq!(
        error.to_string(),
        "session error: Nothing to compact (session too small)"
    );
    assert_eq!(
        compaction_end_errors(&fixture.events()),
        vec!["Compaction failed: Nothing to compact (session too small)".to_owned()]
    );

    // After three rounds, manual compaction succeeds. q2 amplification (~21 tokens)
    // puts the keep_recent=10 cut point at q2 (user = turn start, not a split); the
    // first round is what gets compacted.
    fixture.seed_turn(
        &format!("q1 {}", "x".repeat(80)),
        &format!("a1 {}", "y".repeat(80)),
        100,
    );
    fixture.seed_turn(&format!("q2 {}", "x".repeat(80)), "a2", 100);
    fixture.seed_turn("q3", "a3", 100);
    let result = fixture
        .runner
        .compact(Some("focus on the first turn"))
        .await
        .expect("manual compact succeeds");
    assert!(
        result.summary.starts_with("MANUAL SUMMARY"),
        "unexpected summary: {:?}",
        result.summary
    );
    assert!(result.estimated_tokens_after.is_some());
    let events = fixture.events();
    // First failed start/end + successful start/end.
    assert_eq!(
        event_types(&events),
        vec![
            "compaction_start",
            "compaction_end",
            "compaction_start",
            "compaction_end"
        ]
    );

    // The tail is already a compaction entry: compacting again is rejected.
    let error = fixture
        .runner
        .compact(None)
        .await
        .expect_err("already compacted");
    assert_eq!(error.to_string(), "session error: Already compacted");
    assert_eq!(
        compaction_end_errors(&fixture.events())
            .last()
            .expect("last"),
        "Compaction failed: Already compacted"
    );
}

// ---------------------------------------------------------------------------
// length-stop recovery chain (32850ef7c)
// ---------------------------------------------------------------------------

/// Helper: a length-stop message with output below the model's maxTokens.
/// The fixture model has maxTokens=65536 and contextWindow=8192.
fn length_stop_below_limit(text: &str, output_tokens: u64) -> AssistantMessage {
    let mut message = faux_assistant_message(
        text,
        FauxAssistantOptions {
            stop_reason: Some(StopReason::Length),
            ..Default::default()
        },
    );
    message.usage = serde_json::from_value(json!({
        "input": 100,
        "output": output_tokens,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 100 + output_tokens,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    }))
    .expect("usage");
    message.timestamp = now_ms();
    message
}

/// A length stop below the model's desired output limit is recoverable:
/// it triggers compaction + willRetry → true (agent-session.ts:1990-2001
/// @ 32850ef7c).
#[tokio::test]
async fn length_stop_below_max_tokens_triggers_compaction_and_retry() {
    let mut fixture = fixture(settings(), vec![scripted("RECOVERED")]);
    fixture.seed_turn(
        &format!("q {}", "x".repeat(80)),
        &format!("a {}", "y".repeat(80)),
        100,
    );
    // output=16 < maxTokens=65536 → recoverable length stop.
    let truncated = length_stop_below_limit("partial", 16);
    // Push the truncated message into agent state (like message_end did).
    let mut messages = fixture.agent.state().messages;
    messages.push(AgentMessage::Assistant(truncated.clone()));
    fixture.agent.set_messages(messages);

    assert!(fixture.runner.check_compaction(&truncated, true).await);
    let events = fixture.events();
    // compaction_start + compaction_end with willRetry=true.
    assert_eq!(
        event_types(&events),
        vec!["compaction_start", "compaction_end"]
    );
    match &events[1] {
        CompactionEvent::CompactionEnd {
            reason,
            will_retry,
            aborted,
            ..
        } => {
            assert_eq!(
                *reason,
                rpi::core::compaction_runner::CompactionReason::Overflow
            );
            assert!(will_retry);
            assert!(!aborted);
        }
        other => panic!("expected compaction_end, got {other:?}"),
    }
    // The truncated length-stop message was removed from agent state for the
    // retry. The last message should not be a length-stop assistant.
    let messages = fixture.agent.state().messages;
    if let Some(AgentMessage::Assistant(last)) = messages.last() {
        assert!(
            last.stop_reason != StopReason::Length,
            "agent state should not end with a length-stop after recovery setup"
        );
    }
}

/// A length stop at the model's desired output limit (output == maxTokens)
/// is NOT recoverable: no compaction is triggered (agent-session.ts:1990-2001
/// @ 32850ef7c).
#[tokio::test]
async fn length_stop_at_max_tokens_is_not_recoverable() {
    let mut fixture = fixture(settings(), Vec::new());
    fixture.seed_turn("q", "a", 100);
    // output=65536 == maxTokens → not recoverable.
    let at_limit = length_stop_below_limit("full", 65536);
    assert!(!fixture.runner.check_compaction(&at_limit, true).await);
    assert!(fixture.events().is_empty());
}

/// Two consecutive recoverable length stops: the second emits a
/// recovery-failure event and returns false (one-shot budget).
#[tokio::test]
async fn length_stop_recovery_is_attempted_only_once() {
    let mut fixture = fixture(settings(), vec![scripted("SUMMARY")]);
    fixture.seed_turn(
        &format!("q {}", "x".repeat(80)),
        &format!("a {}", "y".repeat(80)),
        100,
    );
    let first = length_stop_below_limit("first truncated", 16);
    let mut messages = fixture.agent.state().messages;
    messages.push(AgentMessage::Assistant(first.clone()));
    fixture.agent.set_messages(messages);

    // First: recovery compaction + willRetry → true.
    assert!(fixture.runner.check_compaction(&first, true).await);

    // Second: another length stop — recovery budget exhausted.
    let mut second = length_stop_below_limit("second truncated", 8);
    second.timestamp = now_ms() + 1000;
    let mut messages = fixture.agent.state().messages;
    messages.push(AgentMessage::Assistant(second.clone()));
    fixture.agent.set_messages(messages);
    assert!(!fixture.runner.check_compaction(&second, true).await);
    let errors = compaction_end_errors(&fixture.events());
    assert_eq!(
        errors,
        vec![
            "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                .to_owned()
        ]
    );
}

/// After compaction with willRetry, a length-stop message restored from
/// session history is removed from agent state (agent-session.ts:2184-2189
/// @ 32850ef7c).
#[tokio::test]
async fn compaction_retry_removes_length_stop_from_agent_state() {
    let mut fixture = fixture(settings(), vec![scripted("SUMMARY")]);
    fixture.seed_turn(
        &format!("q {}", "x".repeat(80)),
        &format!("a {}", "y".repeat(80)),
        100,
    );
    // Simulate: a length stop was persisted (session) and restored into agent
    // state by finish_compaction's build_session_context.
    let truncated = length_stop_below_limit("truncated", 16);
    let mut messages = fixture.agent.state().messages;
    messages.push(AgentMessage::Assistant(truncated.clone()));
    fixture.agent.set_messages(messages);

    // Run the auto-compaction with willRetry=true.
    assert!(fixture.runner.check_compaction(&truncated, true).await);

    // The recovery path removes the last assistant if it's error/length.
    // Verify that agent state doesn't end with a length-stop assistant.
    let messages = fixture.agent.state().messages;
    if let Some(AgentMessage::Assistant(last)) = messages.last() {
        assert!(
            last.stop_reason != StopReason::Length && last.stop_reason != StopReason::Error,
            "agent state should not end with a length/error stop after retry setup"
        );
    }
}

// ---------------------------------------------------------------------------
// abort-token lifecycle: clear before compaction_end emit (3852cb2b8)
// ---------------------------------------------------------------------------

/// The abort token is cleared *before* the `compaction_end` event is emitted
/// so that `compaction_end` listeners can submit queued prompts and see
/// `isCompacting === false` (agent-session.ts:1907-1908 @ 3852cb2b8).
///
/// The test captures the token state at the moment the sink receives
/// `compaction_end`. If the clear happened after emit, the token would still
/// be `Some` and the assertion would fail.
#[tokio::test]
async fn abort_token_cleared_before_compaction_end_emit_manual_3852cb2b8() {
    let token_at_emit: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));

    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(8192),
            max_tokens: Some(65536),
        }]),
        ..Default::default()
    });
    provider.set_responses(vec![scripted("SUMMARY")]);
    let model = provider.get_model(None).expect("faux-1");

    let agent = Arc::new(Agent::new(AgentOptions::new(provider.stream_fn())));
    let session = SessionManager::in_memory(None, NewSessionOptions::default()).expect("session");

    let events: Arc<Mutex<Vec<CompactionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_sink = events.clone();
    let mut runner = CompactionRunner::new(
        agent.clone(),
        Arc::new(Mutex::new(session)),
        Some(model),
        settings(),
        None,
        provider.stream_fn(),
        ThinkingLevel::Off,
        Arc::new(move |event| {
            events_sink.lock().expect("events").push(event);
        }),
    );

    // Swap the emit sink to one that inspects the abort token cell at the
    // moment compaction_end is delivered.
    let runner_abort_cell = runner.abort_token_cell();
    let cell = runner_abort_cell.clone();
    let token_state = token_at_emit.clone();
    runner.set_emit_sink(Arc::new(move |event| {
        if matches!(event, CompactionEvent::CompactionEnd { .. }) {
            let is_none = cell.lock().unwrap_or_else(|e| e.into_inner()).is_none();
            *token_state.lock().unwrap() = Some(is_none);
        }
    }));

    // Seed enough turns for a cut point (same pattern as the Fixture tests).
    seed_turn_via_runner(
        &mut runner,
        &agent,
        &format!("q1 {}", "x".repeat(80)),
        &format!("a1 {}", "y".repeat(80)),
        100,
    );
    seed_turn_via_runner(
        &mut runner,
        &agent,
        &format!("q2 {}", "x".repeat(80)),
        "a2",
        100,
    );
    seed_turn_via_runner(&mut runner, &agent, "q3", "a3", 100);

    let result = runner.compact(Some("focus")).await;
    assert!(result.is_ok(), "manual compact should succeed");

    let token_state = *token_at_emit.lock().unwrap();
    match token_state {
        Some(false) => panic!(
            "abort token was still Some when compaction_end was emitted — \
             must be cleared before emit (3852cb2b8)"
        ),
        Some(true) => {} // pass
        None => panic!("compaction_end was never emitted"),
    }
}

/// Same check for auto-compaction: the token is cleared before each
/// `compaction_end` emit path (3852cb2b8).
#[tokio::test]
async fn abort_token_cleared_before_auto_compaction_end_emit_3852cb2b8() {
    let token_at_emit: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));

    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(8192),
            max_tokens: Some(65536),
        }]),
        ..Default::default()
    });
    provider.set_responses(vec![scripted("SUMMARY")]);
    let model = provider.get_model(None).expect("faux-1");

    let mut agent_opts = AgentOptions::new(provider.stream_fn());
    agent_opts.initial_state = InitialAgentState {
        model: Some(model.clone()),
        thinking_level: Some(ThinkingLevel::Off),
        ..Default::default()
    };
    let agent = Arc::new(Agent::new(agent_opts));
    let session = SessionManager::in_memory(None, NewSessionOptions::default()).expect("session");

    let events: Arc<Mutex<Vec<CompactionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_sink = events.clone();
    let mut runner = CompactionRunner::new(
        agent.clone(),
        Arc::new(Mutex::new(session)),
        Some(model),
        settings(),
        None,
        provider.stream_fn(),
        ThinkingLevel::Off,
        Arc::new(move |event| {
            events_sink.lock().expect("events").push(event);
        }),
    );

    let runner_abort_cell = runner.abort_token_cell();
    let cell = runner_abort_cell.clone();
    let token_state = token_at_emit.clone();
    runner.set_emit_sink(Arc::new(move |event| {
        if matches!(event, CompactionEvent::CompactionEnd { .. }) {
            let is_none = cell.lock().unwrap_or_else(|e| e.into_inner()).is_none();
            token_state.lock().unwrap().push(is_none);
        }
    }));

    seed_turn_via_runner(
        &mut runner,
        &agent,
        &format!("q {}", "x".repeat(80)),
        &format!("a {}", "y".repeat(80)),
        5000,
    );

    // Trigger auto-compaction via a threshold message (9000 > 8192-4096).
    let msg = assistant("done", StopReason::Stop, 9000, now_ms());
    runner.check_compaction(&msg, true).await;

    let states = token_at_emit.lock().unwrap().clone();
    assert!(
        !states.is_empty(),
        "compaction_end should have been emitted"
    );
    for state in &states {
        assert!(
            *state,
            "abort token must be None when compaction_end is emitted (3852cb2b8)"
        );
    }
}

// ---------------------------------------------------------------------------
// manual compaction race: abort token lifecycle (7253 / e56893f4c)
// ---------------------------------------------------------------------------

/// Manual compaction clears the abort token after completion: a prompt
/// arriving immediately after will not see a stale compaction state.
#[tokio::test]
async fn manual_compact_clears_abort_token_after_completion_7253() {
    let mut fixture = fixture(settings(), vec![scripted("MANUAL SUMMARY")]);
    fixture.seed_turn(
        &format!("q1 {}", "x".repeat(80)),
        &format!("a1 {}", "y".repeat(80)),
        100,
    );
    fixture.seed_turn(&format!("q2 {}", "x".repeat(80)), "a2", 100);
    fixture.seed_turn("q3", "a3", 100);

    let cell = fixture.runner.abort_token_cell();
    assert!(
        cell.lock().unwrap_or_else(|e| e.into_inner()).is_none(),
        "abort token should be None before compaction"
    );

    let _result = fixture.runner.compact(Some("focus")).await;

    assert!(
        cell.lock().unwrap_or_else(|e| e.into_inner()).is_none(),
        "abort token should be None after compaction completes"
    );
}

// ---------------------------------------------------------------------------
// Helper: seed a turn into a runner+agent pair (for standalone runner tests)
// ---------------------------------------------------------------------------

/// Seed a conversation round into the runner's session and sync agent state.
/// Used by standalone-runner tests that don't go through the `Fixture` struct.
fn seed_turn_via_runner(
    runner: &mut CompactionRunner,
    agent: &Arc<Agent>,
    user_text: &str,
    assistant_text: &str,
    total_tokens: u64,
) {
    let usage: rpi_ai::types::Usage = serde_json::from_value(json!({
        "input": total_tokens, "output": 0, "cacheRead": 0, "cacheWrite": 0,
        "totalTokens": total_tokens,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    }))
    .expect("usage");
    let mut assistant_msg = faux_assistant_message(assistant_text, FauxAssistantOptions::default());
    assistant_msg.usage = usage;
    assistant_msg.timestamp = now_ms();
    let user_msg: AgentMessage = serde_json::from_value(json!({
        "role": "user",
        "content": user_text,
        "timestamp": now_ms(),
    }))
    .expect("user msg");

    runner
        .session_mut()
        .append_message(user_msg.clone())
        .expect("append user");
    runner
        .session_mut()
        .append_message(AgentMessage::Assistant(assistant_msg.clone()))
        .expect("append assistant");
    let mut messages = agent.state().messages;
    messages.push(user_msg);
    messages.push(AgentMessage::Assistant(assistant_msg));
    agent.set_messages(messages);
}
