//! T08 parity: `fixtures/generated/compaction-{threshold,overflow}/`
//! (recorded from the upstream `createAgentSession` + faux) vs the same
//! script driven by `rpi::core::compaction_runner::CompactionRunner`.
//!
//! The tests replicate the minimal agent-session wiring (`_handleAgentEvent`
//! persistence + `_runAgentPrompt`'s post-run check loop + the pre-prompt
//! checks, agent-session.ts:595-665/1061-1103/1197-1202) — that wiring is
//! absorbed by T10's AgentSession; it is replicated here only for parity.
//!
//! Normalization conventions (on top of the `rpi_test_support`
//! Normalizer):
//! - the whole `usage` key is stripped: faux usage is estimated from the
//!   full context (including the system prompt) and double-counts
//!   prompt-cache; the upstream fixture system prompt is a coding-agent
//!   builder artifact (only ported in T12), and rpi pads a fixed-length
//!   system prompt, so the numbers are not comparable.
//! - `tokensBefore` / `estimatedTokensAfter` likewise (derived from the
//!   usage anchor). The compaction *decision* (when to compact, the cut
//!   point, summary content, firstKeptEntryId, willRetry,
//!   event-type order) stays in the contract and is compared value by
//!   value.
//! - the session header `cwd` is replaced with a placeholder (temp-dir
//!   path).
//! - event comparison takes only the compaction event subset
//!   (compaction_start/compaction_end/summarization_retry_*); Agent-layer
//!   events are not part of the runner contract.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi::core::compaction_runner::{CompactionEvent, CompactionRunner};
use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi_agent::compaction::CompactionSettings;
use rpi_agent::messages::AgentMessage;
use rpi_agent::types::{AgentEvent, ThinkingLevel};
use rpi_agent::{Agent, AgentOptions, InitialAgentState};
use rpi_ai::types::{AssistantMessage, StopReason};
use rpi_test_support::diff::diff_jsonl;
use rpi_test_support::faux::{
    faux_assistant_message, FauxAssistantOptions, FauxModelDefinition, FauxProvider,
    FauxProviderOptions, FauxResponseStep,
};
use serde_json::Value;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rpi-parity-compaction-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        TestDir(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Fixed-length padding system prompt (~1000 tokens): triggering the threshold needs
/// overall usage comparable to upstream, but the concrete text is not part of the
/// contract (usage numbers are stripped).
fn filler_system_prompt() -> String {
    "fixture system prompt filler. ".repeat(140)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Scripted responses: fresh timestamps at call time (upstream factory-response
/// semantics). Fixed/zero timestamps would be stopped by the stale-usage guard
/// (agent-session.ts:1974), silently breaking the compaction chain.
///
/// Anti-collision: under a millisecond clock, if a compaction entry write (real clock)
/// and the next scripted response land in the same millisecond, the
/// `assistantMessage.timestamp <= compactionTs` guard would wrongly kill subsequent
/// checks (parallel tests widen this window). The factory yields 5ms first, then takes
/// strictly increasing timestamps.
fn scripted(content: impl Into<String>) -> FauxResponseStep {
    static LAST_TS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let content = content.into();
    FauxResponseStep::Factory(Box::new(move |_context, _options, _state, _model| {
        std::thread::sleep(std::time::Duration::from_millis(5));
        let now = now_ms();
        // CAS loop: strictly increasing timestamps (`fetch_update` reports
        // the *previous* value, which would shift the sequence by one).
        let timestamp = loop {
            let last = LAST_TS.load(std::sync::atomic::Ordering::SeqCst);
            let next = last.max(now - 1) + 1;
            if LAST_TS
                .compare_exchange(
                    last,
                    next,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                break next;
            }
        };
        let mut message = faux_assistant_message(content.clone(), FauxAssistantOptions::default());
        message.timestamp = timestamp;
        message
    }))
}

fn scripted_overflow_error() -> FauxResponseStep {
    FauxResponseStep::Factory(Box::new(move |_context, _options, _state, _model| {
        let mut message = faux_assistant_message(
            "",
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("prompt is too long: 200000 tokens > 16384 maximum".to_owned()),
                ..Default::default()
            },
        );
        message.timestamp = now_ms();
        message
    }))
}

// ---------------------------------------------------------------------------
// Harness: a replica of the minimal agent-session wiring
// ---------------------------------------------------------------------------

struct Harness {
    agent: Arc<Agent>,
    runner: CompactionRunner,
    agent_events: Arc<Mutex<Vec<AgentEvent>>>,
    compaction_events: Arc<Mutex<Vec<CompactionEvent>>>,
    /// `_lastAssistantMessage` (agent-session.ts:647): the consumption source of the
    /// post-run check.
    last_assistant: Option<AssistantMessage>,
}

impl Harness {
    fn new(
        dir: &Path,
        context_window: u32,
        settings: CompactionSettings,
        responses: Vec<FauxResponseStep>,
    ) -> Self {
        let provider = FauxProvider::new(FauxProviderOptions {
            models: Some(vec![FauxModelDefinition {
                id: "faux-1".to_owned(),
                name: None,
                reasoning: None,
                input: None,
                cost: None,
                context_window: Some(context_window),
                max_tokens: Some(65536),
            }]),
            ..Default::default()
        });
        provider.set_responses(responses);
        let model = provider.get_model(None).expect("faux-1 registered");

        let mut options = AgentOptions::new(provider.stream_fn());
        options.initial_state = InitialAgentState {
            system_prompt: Some(filler_system_prompt()),
            model: Some(model.clone()),
            thinking_level: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let mut agent = Agent::new(options);
        // agent-session passes the session id through to the provider (the key the
        // prompt-cache mock keys on).
        agent.session_id = Some("parity-compaction-session".to_owned());
        let agent = Arc::new(agent);

        let agent_events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let listener_events = agent_events.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _signal| {
            let listener_events = listener_events.clone();
            Box::pin(async move {
                listener_events.lock().expect("events").push(event);
            })
        }));
        std::mem::forget(unsubscribe);

        let session_dir = dir.join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let session = SessionManager::create(dir, Some(&session_dir), NewSessionOptions::default())
            .expect("create session");
        let compaction_events: Arc<Mutex<Vec<CompactionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = compaction_events.clone();
        let runner = CompactionRunner::new(
            agent.clone(),
            Arc::new(Mutex::new(session)),
            Some(model),
            settings,
            None,
            provider.stream_fn(),
            ThinkingLevel::Off,
            Arc::new(move |event| sink_events.lock().expect("events").push(event)),
        );

        let harness = Self {
            agent,
            runner,
            agent_events,
            compaction_events,
            last_assistant: None,
        };
        // createAgentSession's initial entry order (fixture lines 2/3).
        harness
            .runner
            .session_mut()
            .append_model_change("faux", "faux-1")
            .expect("append model_change");
        harness
            .runner
            .session_mut()
            .append_thinking_level_change("off")
            .expect("append thinking_level_change");
        harness
    }

    /// The persistence part of `_handleAgentEvent` (agent-session.ts:624-652):
    /// message_end persists user/assistant/toolResult; a user-turn start and a
    /// non-error assistant reply reset the overflow recovery budget; tracks
    /// `_lastAssistantMessage`.
    fn drain_agent_events(&mut self) {
        let events: Vec<AgentEvent> =
            std::mem::take(&mut *self.agent_events.lock().expect("events"));
        for event in events {
            match event {
                AgentEvent::MessageStart { message, .. } => {
                    if matches!(message, AgentMessage::User(_)) {
                        self.runner.reset_overflow_recovery();
                    }
                }
                AgentEvent::MessageEnd { message, .. } => match &message {
                    AgentMessage::User(_) | AgentMessage::ToolResult(_) => {
                        self.runner
                            .session_mut()
                            .append_message(message.clone())
                            .expect("append message");
                    }
                    AgentMessage::Assistant(assistant) => {
                        self.runner
                            .session_mut()
                            .append_message(message.clone())
                            .expect("append message");
                        if assistant.stop_reason != StopReason::Error {
                            self.runner.reset_overflow_recovery();
                        }
                        self.last_assistant = Some(assistant.clone());
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    /// `_runAgentPrompt` (agent-session.ts:1061-1073) + the pre-prompt submission
    /// check (:1197-1202). The pre-prompt check does not continue.
    async fn prompt(&mut self, text: &str) {
        if let Some(assistant) = self.find_last_assistant_in_state() {
            self.runner.check_compaction(&assistant, false).await;
        }

        self.agent.prompt(text).await.expect("prompt resolves");
        self.drain_agent_events();

        // `_handlePostAgentRun` loop: check → continue → check → …
        while let Some(assistant) = self.last_assistant.take() {
            if self.runner.check_compaction(&assistant, true).await {
                self.agent.continue_run().await.expect("continue resolves");
                self.drain_agent_events();
            } else {
                break;
            }
        }
    }

    /// `_findLastAssistantMessage`（agent-session.ts:684-693）。
    fn find_last_assistant_in_state(&self) -> Option<AssistantMessage> {
        self.agent
            .state()
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                AgentMessage::Assistant(a) => Some(a.clone()),
                _ => None,
            })
    }
}

// ---------------------------------------------------------------------------
// Comparison preparation
// ---------------------------------------------------------------------------

const STRIPPED_KEYS: &[&str] = &["usage", "tokensBefore", "estimatedTokensAfter"];
const COMPACTION_EVENT_TYPES: &[&str] = &[
    "compaction_start",
    "compaction_end",
    "summarization_retry_scheduled",
    "summarization_retry_attempt_start",
    "summarization_retry_finished",
];

fn strip_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !STRIPPED_KEYS.contains(&key.as_str()));
            for (_, val) in map.iter_mut() {
                strip_keys(val);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_keys(item);
            }
        }
        _ => {}
    }
}

/// session.jsonl line-by-line preparation: strip usage coefficient values, placeholder
/// cwd, re-render as JSONL.
fn prepare_session_lines(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_str(line).expect("session line is JSON");
        if value.get("type").and_then(Value::as_str) == Some("session") {
            value["cwd"] = Value::from("<cwd>");
        }
        strip_keys(&mut value);
        out.push_str(&serde_json::to_string(&value).expect("render"));
        out.push('\n');
    }
    out
}

/// events line-by-line preparation: keep only the compaction event subset, strip usage
/// coefficient values.
fn prepare_event_lines(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_str(line).expect("event line is JSON");
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if !COMPACTION_EVENT_TYPES.contains(&event_type) {
            continue;
        }
        strip_keys(&mut value);
        out.push_str(&serde_json::to_string(&value).expect("render"));
        out.push('\n');
    }
    out
}

fn compare(harness: &Harness, scenario: &str) {
    // Session file parity.
    let session_file = harness
        .runner
        .session()
        .get_session_file()
        .map(|path| path.to_path_buf())
        .expect("file-backed session");
    let actual_session = std::fs::read_to_string(session_file).expect("read actual session");
    let expected_session =
        std::fs::read_to_string(fixtures_dir().join(scenario).join("session.jsonl"))
            .expect("read fixture session");
    diff_jsonl(
        &prepare_session_lines(&expected_session),
        &prepare_session_lines(&actual_session),
    )
    .unwrap_or_else(|f| panic!("{scenario}: session parity diff:\n{f}"));

    // Compaction event parity.
    let mut actual_events = String::new();
    for event in harness.compaction_events.lock().expect("events").iter() {
        actual_events.push_str(&serde_json::to_string(event).expect("serialize event"));
        actual_events.push('\n');
    }
    let expected_events =
        std::fs::read_to_string(fixtures_dir().join(scenario).join("events.jsonl"))
            .expect("read fixture events");
    diff_jsonl(
        &prepare_event_lines(&expected_events),
        &prepare_event_lines(&actual_events),
    )
    .unwrap_or_else(|f| panic!("{scenario}: compaction event parity diff:\n{f}"));
}

// ---------------------------------------------------------------------------
// Scenarios (scripts mirror fixtures/generate-fixtures.mjs)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parity_compaction_threshold() {
    async fn scenario() {
        let dir = TestDir::new("threshold");
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 4096,
            keep_recent_tokens: 512,
        };
        let responses = vec![
            scripted(format!("ALPHA {}", "alpha evidence block. ".repeat(560))),
            scripted("Turn prefix summary: the user asked about the alpha topic."),
            scripted(format!("BETA {}", "beta evidence block. ".repeat(560))),
            scripted("Updated history summary: alpha and beta evidence discussed."),
            scripted("Turn prefix summary: the user then asked about beta."),
            scripted("A short final answer."),
        ];
        let mut harness = Harness::new(&dir.0, 8192, settings, responses);
        harness
            .prompt("First question about the alpha topic.")
            .await;
        harness
            .prompt("Second question about the beta topic.")
            .await;
        harness.prompt("A small follow-up question.").await;
        compare(&harness, "compaction-threshold");
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test]
async fn parity_compaction_overflow() {
    async fn scenario() {
        let dir = TestDir::new("overflow");
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 8192,
            keep_recent_tokens: 256,
        };
        let responses = vec![
            scripted(format!("FIRST {}", "first answer block. ".repeat(80))),
            scripted(format!("SECOND {}", "second answer block. ".repeat(80))),
            scripted_overflow_error(),
            scripted("History summary: two answers were given before the overflow."),
            scripted("Turn prefix summary: the user asked the overflowing question."),
            scripted("Recovered answer after compaction and retry."),
        ];
        let mut harness = Harness::new(&dir.0, 16384, settings, responses);
        harness.prompt("Question one.").await;
        harness.prompt("Question two.").await;
        harness
            .prompt("The question that overflows the context window.")
            .await;
        compare(&harness, "compaction-overflow");
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}
