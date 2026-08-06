//! Port of `external/pi/packages/agent/test/agent.test.ts` @ 2efa728.
//!
//! Structural mapping notes:
//! - The TS-only "default stream function" case is not ported (Rust always
//!   injects `StreamFn`).
//! - Upstream's "thrown run failure" test uses a `streamFn` that throws
//!   synchronously. The Rust `StreamFn` contract encodes failures as stream
//!   events instead (stream-fn.rs), so the same observable sequence is
//!   produced by a stream whose only event is an `error` event.
//! - Upstream mutates `agent.state.*` in place; Rust uses the setters
//!   (`set_system_prompt`, ...). Copy semantics of `set_tools`/`set_messages`
//!   are asserted via clone-then-mutate.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::StreamExt;
use pir_agent::agent::AgentListener;
use pir_agent::messages::AgentMessage;
use pir_agent::stream_fn::StreamFn;
use pir_agent::types::{
    AgentEvent, AgentTool, AgentToolResult, AgentToolUpdateCallback, ThinkingLevel,
};
use pir_agent::{Agent, AgentOptions, InitialAgentState};
use pir_ai::types::{
    ApiKind, AssistantContent, AssistantMessage, AssistantRole, ErrorReason, StopReason,
    StreamEvent, TextContent, ToolResultContent, Usage, UserContent, UserMessage, UserRole,
};
use pir_test_support::faux::{
    faux_assistant_message, FauxAssistantOptions, FauxProvider, FauxProviderOptions,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn unused_stream_fn() -> StreamFn {
    Arc::new(|_model, _context, _options| panic!("Unexpected stream call"))
}

fn assistant_text(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        api: ApiKind::from("openai-responses"),
        provider: "openai".to_owned(),
        model: "mock".to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 2,
    }
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Text(text.to_owned()),
        timestamp: 1,
    })
}

fn faux_provider() -> Arc<FauxProvider> {
    FauxProvider::new(FauxProviderOptions::default())
}

fn text_response(text: &str) -> pir_test_support::faux::FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

fn tool_use_response(
    tool_calls: Vec<AssistantContent>,
) -> pir_test_support::faux::FauxResponseStep {
    faux_assistant_message(
        tool_calls,
        FauxAssistantOptions {
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        },
    )
    .into()
}

fn tool_call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(pir_ai::types::ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.as_object().expect("arguments object").clone(),
        thought_signature: None,
    })
}

/// Stream fn that streams `start` and then waits for the abort signal,
/// answering with an aborted error event (upstream abort-polling mock).
fn abortable_stream_fn() -> StreamFn {
    Arc::new(|_model, _context, options| {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
        tokio::spawn(async move {
            let partial = assistant_text("");
            let _ = tx.send(StreamEvent::Start {
                partial: partial.clone(),
            });
            loop {
                if options.signal.as_ref().is_some_and(|s| s.is_cancelled()) {
                    let mut aborted = partial;
                    aborted.stop_reason = StopReason::Aborted;
                    aborted.error_message = Some("Request was aborted".to_owned());
                    let _ = tx.send(StreamEvent::Error {
                        reason: ErrorReason::Aborted,
                        error: aborted,
                    });
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed()
    })
}

fn collecting_listener() -> (AgentListener, Arc<Mutex<Vec<AgentEvent>>>) {
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let listener_events = events.clone();
    let listener: AgentListener = Arc::new(move |event, _signal| {
        let listener_events = listener_events.clone();
        Box::pin(async move {
            listener_events.lock().unwrap().push(event);
        })
    });
    (listener, events)
}

fn event_type(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
    }
}

// ---------------------------------------------------------------------------
// Minimal scripted tool
// ---------------------------------------------------------------------------

type ExecuteFn = Arc<
    dyn Fn(
            Value,
            Option<AgentToolUpdateCallback>,
        ) -> BoxFuture<'static, Result<AgentToolResult, pir_agent::AgentError>>
        + Send
        + Sync,
>;

struct TestTool {
    name: String,
    parameters: Value,
    execute_fn: ExecuteFn,
}

impl TestTool {
    fn new(name: &str, execute_fn: ExecuteFn) -> Self {
        Self {
            name: name.to_owned(),
            parameters: json!({ "type": "object", "properties": {} }),
            execute_fn,
        }
    }
}

#[async_trait]
impl AgentTool for TestTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn label(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "test tool"
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, pir_agent::AgentError> {
        (self.execute_fn)(params, on_update).await
    }
}

fn done_result(status: &str, terminate: bool) -> Result<AgentToolResult, pir_agent::AgentError> {
    Ok(AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text: status.to_owned(),
            text_signature: None,
        })],
        details: json!({ "status": status }),
        terminate: Some(terminate),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Construction / state
// ---------------------------------------------------------------------------

#[test]
fn creates_agent_with_default_state() {
    let agent = Agent::new(AgentOptions::new(unused_stream_fn()));
    let state = agent.state();
    assert_eq!(state.system_prompt, "");
    assert_eq!(state.model.id, "unknown");
    assert_eq!(state.thinking_level, ThinkingLevel::Off);
    assert!(state.tools.is_empty());
    assert!(state.messages.is_empty());
    assert!(!state.is_streaming);
    assert!(state.streaming_message.is_none());
    assert!(state.pending_tool_calls.is_empty());
    assert!(state.error_message.is_none());
}

#[test]
fn creates_agent_with_custom_initial_state() {
    let provider = faux_provider();
    let model = provider.get_model(None).expect("faux model");
    let mut options = AgentOptions::new(unused_stream_fn());
    options.initial_state = InitialAgentState {
        system_prompt: Some("You are a helpful assistant.".to_owned()),
        model: Some(model.clone()),
        thinking_level: Some(ThinkingLevel::Low),
        ..Default::default()
    };
    let agent = Agent::new(options);
    let state = agent.state();
    assert_eq!(state.system_prompt, "You are a helpful assistant.");
    assert_eq!(state.model, model);
    assert_eq!(state.thinking_level, ThinkingLevel::Low);
}

#[test]
fn subscribes_to_events_and_mutators_do_not_emit() {
    let agent = Agent::new(AgentOptions::new(unused_stream_fn()));
    let event_count = Arc::new(AtomicU64::new(0));
    let unsubscribe = agent.subscribe({
        let event_count = event_count.clone();
        Arc::new(move |_event, _signal| {
            let event_count = event_count.clone();
            Box::pin(async move {
                event_count.fetch_add(1, Ordering::SeqCst);
            })
        })
    });

    // No initial event on subscribe; state mutators don't emit events.
    assert_eq!(event_count.load(Ordering::SeqCst), 0);
    agent.set_system_prompt("Test prompt".to_owned());
    assert_eq!(event_count.load(Ordering::SeqCst), 0);
    assert_eq!(agent.state().system_prompt, "Test prompt");

    unsubscribe();
    agent.set_system_prompt("Another prompt".to_owned());
    assert_eq!(event_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn emits_full_lifecycle_events_for_run_failures() {
    // Upstream: `streamFn: () => { throw new Error("provider exploded") }`.
    // The Rust StreamFn encodes the failure as an error stream event; the
    // observable lifecycle sequence is identical.
    let stream_fn: StreamFn = Arc::new(|_model, _context, _options| {
        let mut error = assistant_text("");
        error.stop_reason = StopReason::Error;
        error.error_message = Some("provider exploded".to_owned());
        futures::stream::iter(vec![StreamEvent::Error {
            reason: ErrorReason::Error,
            error,
        }])
        .boxed()
    });
    let agent = Agent::new(AgentOptions::new(stream_fn));
    let (listener, events) = collecting_listener();
    let _unsubscribe = agent.subscribe(listener);

    tokio::time::timeout(TEST_TIMEOUT, agent.prompt("hello"))
        .await
        .expect("prompt must not hang")
        .expect("prompt resolves");

    let types: Vec<&str> = events.lock().unwrap().iter().map(event_type).collect();
    assert_eq!(
        types,
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
    let state = agent.state();
    let last = state.messages.last().expect("failure message recorded");
    let AgentMessage::Assistant(assistant) = last else {
        panic!("expected assistant message, got {last:?}");
    };
    assert_eq!(assistant.stop_reason, StopReason::Error);
    assert_eq!(
        assistant.error_message.as_deref(),
        Some("provider exploded")
    );
    assert_eq!(state.error_message.as_deref(), Some("provider exploded"));
}

// ---------------------------------------------------------------------------
// Listener barrier
// ---------------------------------------------------------------------------

#[tokio::test]
async fn awaits_async_subscribers_before_prompt_resolves() {
    async fn scenario() {
        let provider = faux_provider();
        provider.set_responses(vec![text_response("ok")]);
        let agent = Arc::new(Agent::new(AgentOptions::new(provider.stream_fn())));

        let barrier = Arc::new(tokio::sync::Notify::new());
        let listener_finished = Arc::new(AtomicBool::new(false));
        let _unsubscribe = agent.subscribe({
            let barrier = barrier.clone();
            let listener_finished = listener_finished.clone();
            Arc::new(move |event, _signal| {
                let barrier = barrier.clone();
                let listener_finished = listener_finished.clone();
                Box::pin(async move {
                    if matches!(event, AgentEvent::AgentEnd { .. }) {
                        barrier.notified().await;
                        listener_finished.store(true, Ordering::SeqCst);
                    }
                })
            })
        });

        let prompt_agent = agent.clone();
        let prompt_handle = tokio::spawn(async move { prompt_agent.prompt("hello").await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!prompt_handle.is_finished());
        assert!(!listener_finished.load(Ordering::SeqCst));
        assert!(agent.is_streaming());

        barrier.notify_one();
        prompt_handle
            .await
            .expect("prompt task")
            .expect("prompt resolves");

        assert!(listener_finished.load(Ordering::SeqCst));
        assert!(!agent.is_streaming());
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test]
async fn wait_for_idle_waits_for_async_subscribers() {
    async fn scenario() {
        let provider = faux_provider();
        provider.set_responses(vec![text_response("ok")]);
        let agent = Arc::new(Agent::new(AgentOptions::new(provider.stream_fn())));

        let barrier = Arc::new(tokio::sync::Notify::new());
        let _unsubscribe = agent.subscribe({
            let barrier = barrier.clone();
            Arc::new(move |event, _signal| {
                let barrier = barrier.clone();
                Box::pin(async move {
                    if matches!(
                        event,
                        AgentEvent::MessageEnd {
                            message: AgentMessage::Assistant(_)
                        }
                    ) {
                        barrier.notified().await;
                    }
                })
            })
        });

        let prompt_agent = agent.clone();
        let prompt_handle = tokio::spawn(async move { prompt_agent.prompt("hello").await });
        let idle_agent = agent.clone();
        let idle_handle = tokio::spawn(async move { idle_agent.wait_for_idle().await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!idle_handle.is_finished());
        assert!(agent.is_streaming());

        barrier.notify_one();
        prompt_handle
            .await
            .expect("prompt task")
            .expect("prompt resolves");
        idle_handle.await.expect("idle task");

        assert!(!agent.is_streaming());
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

// ---------------------------------------------------------------------------
// Abort signal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn passes_active_abort_signal_to_subscribers() {
    async fn scenario() {
        let agent = Arc::new(Agent::new(AgentOptions::new(abortable_stream_fn())));
        let received: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
        let _unsubscribe = agent.subscribe({
            let received = received.clone();
            Arc::new(move |event, signal| {
                let received = received.clone();
                Box::pin(async move {
                    if matches!(event, AgentEvent::AgentStart) {
                        *received.lock().unwrap() = Some(signal);
                    }
                })
            })
        });

        let prompt_agent = agent.clone();
        let prompt_handle = tokio::spawn(async move { prompt_agent.prompt("hello").await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        {
            let signal = received.lock().unwrap();
            let signal = signal.as_ref().expect("listener received a signal");
            assert!(!signal.is_cancelled());
        }

        agent.abort();
        prompt_handle
            .await
            .expect("prompt task")
            .expect("prompt resolves");

        let signal = received.lock().unwrap();
        let signal = signal.as_ref().expect("listener received a signal");
        assert!(signal.is_cancelled());
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[test]
fn abort_without_active_run_does_not_throw() {
    let agent = Agent::new(AgentOptions::new(unused_stream_fn()));
    agent.abort();
    assert!(agent.signal().is_none());
}

// ---------------------------------------------------------------------------
// Tool update settle semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ignores_tool_updates_after_execution_settles() {
    async fn scenario() {
        let saved_update: Arc<Mutex<Option<AgentToolUpdateCallback>>> = Arc::new(Mutex::new(None));
        let tool = TestTool::new(
            "delayed_tool",
            Arc::new({
                let saved_update = saved_update.clone();
                move |_params, on_update| {
                    let saved_update = saved_update.clone();
                    Box::pin(async move {
                        if let Some(on_update) = on_update {
                            on_update(AgentToolResult {
                                content: vec![ToolResultContent::Text(TextContent {
                                    text: "running".to_owned(),
                                    text_signature: None,
                                })],
                                details: json!({ "status": "running" }),
                                ..Default::default()
                            });
                            *saved_update.lock().unwrap() = Some(on_update);
                        }
                        done_result("done", true)
                    })
                }
            }),
        );

        let provider = faux_provider();
        provider.set_responses(vec![tool_use_response(vec![tool_call(
            "call-1",
            "delayed_tool",
            json!({}),
        )])]);
        let mut options = AgentOptions::new(provider.stream_fn());
        options.initial_state = InitialAgentState {
            tools: Some(vec![Arc::new(tool)]),
            ..Default::default()
        };
        let agent = Agent::new(options);
        let (listener, events) = collecting_listener();
        let _unsubscribe = agent.subscribe(listener);

        agent.prompt("run tool").await.expect("prompt resolves");
        let count_after_prompt = events.lock().unwrap().len();

        // A late update after execute settled must be dropped silently.
        if let Some(on_update) = saved_update.lock().unwrap().as_ref() {
            on_update(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent {
                    text: "late".to_owned(),
                    text_signature: None,
                })],
                details: json!({ "status": "late" }),
                ..Default::default()
            });
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let events = events.lock().unwrap();
        assert_eq!(events.len(), count_after_prompt);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
                .count(),
            1
        );
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test]
async fn ignores_settled_parallel_tool_update_while_another_tool_is_running() {
    async fn scenario() {
        let saved_update: Arc<Mutex<Option<AgentToolUpdateCallback>>> = Arc::new(Mutex::new(None));
        let settled_tool = TestTool::new(
            "settled_tool",
            Arc::new({
                let saved_update = saved_update.clone();
                move |_params, on_update| {
                    let saved_update = saved_update.clone();
                    Box::pin(async move {
                        *saved_update.lock().unwrap() = on_update;
                        done_result("done", true)
                    })
                }
            }),
        );

        let slow_started = Arc::new(tokio::sync::Notify::new());
        let release_slow = Arc::new(tokio::sync::Notify::new());
        let slow_tool = TestTool::new(
            "slow_tool",
            Arc::new({
                let slow_started = slow_started.clone();
                let release_slow = release_slow.clone();
                move |_params, _on_update| {
                    let slow_started = slow_started.clone();
                    let release_slow = release_slow.clone();
                    Box::pin(async move {
                        slow_started.notify_one();
                        release_slow.notified().await;
                        done_result("done", true)
                    })
                }
            }),
        );

        let provider = faux_provider();
        provider.set_responses(vec![tool_use_response(vec![
            tool_call("call-1", "settled_tool", json!({})),
            tool_call("call-2", "slow_tool", json!({})),
        ])]);
        let mut options = AgentOptions::new(provider.stream_fn());
        options.initial_state = InitialAgentState {
            tools: Some(vec![Arc::new(settled_tool), Arc::new(slow_tool)]),
            ..Default::default()
        };
        let agent = Arc::new(Agent::new(options));
        let (listener, events) = collecting_listener();
        let _unsubscribe = agent.subscribe(listener);

        let prompt_agent = agent.clone();
        let prompt_handle = tokio::spawn(async move { prompt_agent.prompt("run tools").await });

        // Wait for the slow tool to start and the settled tool to end.
        slow_started.notified().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let settled_ended = events.lock().unwrap().iter().any(|e| {
                    matches!(
                        e,
                        AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "call-1"
                    )
                });
                if settled_ended {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("settled tool must end");
        let count_before_late_update = events.lock().unwrap().len();

        if let Some(on_update) = saved_update.lock().unwrap().as_ref() {
            on_update(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent {
                    text: "late".to_owned(),
                    text_signature: None,
                })],
                details: json!({ "status": "late" }),
                ..Default::default()
            });
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(events.lock().unwrap().len(), count_before_late_update);

        release_slow.notify_one();
        prompt_handle
            .await
            .expect("prompt task")
            .expect("prompt resolves");
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
                .count(),
            0
        );
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

// ---------------------------------------------------------------------------
// State mutators / queues
// ---------------------------------------------------------------------------

#[test]
fn updates_state_with_mutators() {
    let provider = faux_provider();
    let agent = Agent::new(AgentOptions::new(unused_stream_fn()));

    agent.set_system_prompt("Custom prompt".to_owned());
    assert_eq!(agent.state().system_prompt, "Custom prompt");

    let model = provider.get_model(None).expect("faux model");
    agent.set_model(model.clone());
    assert_eq!(agent.state().model, model);

    agent.set_thinking_level(ThinkingLevel::High);
    assert_eq!(agent.state().thinking_level, ThinkingLevel::High);

    // set_tools copies: mutating the source vec afterwards must not leak.
    let tool: Arc<dyn AgentTool> = Arc::new(TestTool::new(
        "test",
        Arc::new(|_, _| Box::pin(async { done_result("ok", false) })),
    ));
    let mut tools = vec![tool];
    agent.set_tools(tools.clone());
    assert_eq!(agent.state().tools.len(), 1);
    tools.push(Arc::new(TestTool::new(
        "other",
        Arc::new(|_, _| Box::pin(async { done_result("ok", false) })),
    )));
    assert_eq!(agent.state().tools.len(), 1);

    // set_messages copies.
    let mut messages = vec![user_message("Hello")];
    agent.set_messages(messages.clone());
    assert_eq!(agent.state().messages.len(), 1);
    messages.push(user_message("leaked?"));
    assert_eq!(agent.state().messages.len(), 1);

    // append via snapshot + set, then clear.
    let mut snapshot = agent.state().messages;
    snapshot.push(AgentMessage::Assistant(assistant_text("Hi")));
    agent.set_messages(snapshot);
    assert_eq!(agent.state().messages.len(), 2);
    agent.set_messages(Vec::new());
    assert!(agent.state().messages.is_empty());
}

#[test]
fn queues_steering_message_without_touching_state_messages() {
    let agent = Agent::new(AgentOptions::new(unused_stream_fn()));
    agent.steer(user_message("Steering message"));
    // Queued, but not yet in state.messages.
    assert!(agent.state().messages.is_empty());
    assert!(agent.has_queued_messages());
}

#[test]
fn queues_follow_up_message_without_touching_state_messages() {
    let agent = Agent::new(AgentOptions::new(unused_stream_fn()));
    agent.follow_up(user_message("Follow-up message"));
    assert!(agent.state().messages.is_empty());
    assert!(agent.has_queued_messages());
}

// ---------------------------------------------------------------------------
// Concurrency guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn throws_when_prompt_called_while_streaming() {
    async fn scenario() {
        let agent = Arc::new(Agent::new(AgentOptions::new(abortable_stream_fn())));
        let prompt_agent = agent.clone();
        let first_prompt = tokio::spawn(async move { prompt_agent.prompt("First message").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(agent.is_streaming());

        let error = agent
            .prompt("Second message")
            .await
            .expect_err("second prompt must fail");
        assert_eq!(
            error.to_string(),
            "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
        );

        agent.abort();
        first_prompt
            .await
            .expect("prompt task")
            .expect("first prompt resolves");
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test]
async fn throws_when_continue_called_while_streaming() {
    async fn scenario() {
        let agent = Arc::new(Agent::new(AgentOptions::new(abortable_stream_fn())));
        let prompt_agent = agent.clone();
        let first_prompt = tokio::spawn(async move { prompt_agent.prompt("First message").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(agent.is_streaming());

        let error = agent
            .continue_run()
            .await
            .expect_err("continue while streaming must fail");
        assert_eq!(
            error.to_string(),
            "Agent is already processing. Wait for completion before continuing."
        );

        agent.abort();
        first_prompt
            .await
            .expect("prompt task")
            .expect("first prompt resolves");
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

// ---------------------------------------------------------------------------
// continue() degradation chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn continue_processes_queued_follow_up_after_assistant_turn() {
    let provider = faux_provider();
    provider.set_responses(vec![text_response("Processed")]);
    let agent = Agent::new(AgentOptions::new(provider.stream_fn()));

    agent.set_messages(vec![
        user_message("Initial"),
        AgentMessage::Assistant(assistant_text("Initial response")),
    ]);
    agent.follow_up(user_message("Queued follow-up"));

    tokio::time::timeout(TEST_TIMEOUT, agent.continue_run())
        .await
        .expect("continue must not hang")
        .expect("continue resolves");

    let state = agent.state();
    let has_follow_up = state.messages.iter().any(|m| match m {
        AgentMessage::User(u) => {
            matches!(&u.content, UserContent::Text(t) if t == "Queued follow-up")
        }
        _ => false,
    });
    assert!(has_follow_up);
    assert!(matches!(
        state.messages.last(),
        Some(AgentMessage::Assistant(_))
    ));
}

#[tokio::test]
async fn continue_keeps_one_at_a_time_steering_from_assistant_tail() {
    let provider = faux_provider();
    provider.set_responses(vec![
        faux_assistant_message("Processed 1", FauxAssistantOptions::default()).into(),
        faux_assistant_message("Processed 2", FauxAssistantOptions::default()).into(),
    ]);
    let agent = Agent::new(AgentOptions::new(provider.stream_fn()));

    agent.set_messages(vec![
        user_message("Initial"),
        AgentMessage::Assistant(assistant_text("Initial response")),
    ]);
    agent.steer(user_message("Steering 1"));
    agent.steer(user_message("Steering 2"));

    tokio::time::timeout(TEST_TIMEOUT, agent.continue_run())
        .await
        .expect("continue must not hang")
        .expect("continue resolves");

    let state = agent.state();
    let roles: Vec<&str> = state.messages[state.messages.len() - 4..]
        .iter()
        .map(|m| match m {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            _ => "other",
        })
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert_eq!(provider.call_count(), 2);
}

// ---------------------------------------------------------------------------
// prepareNextTurn / sessionId
// ---------------------------------------------------------------------------

#[tokio::test]
async fn keeps_legacy_prepare_next_turn_signal_callback_behavior() {
    let tool = TestTool::new(
        "noop",
        Arc::new(|_params, _on_update| Box::pin(async { done_result("ok", false) })),
    );

    let provider = faux_provider();
    provider.set_responses(vec![
        tool_use_response(vec![tool_call("tool-1", "noop", json!({}))]),
        text_response("done"),
    ]);
    let mut options = AgentOptions::new(provider.stream_fn());
    options.initial_state = InitialAgentState {
        tools: Some(vec![Arc::new(tool)]),
        ..Default::default()
    };
    let saw_abort_signal = Arc::new(AtomicBool::new(false));
    options.prepare_next_turn = Some({
        let saw_abort_signal = saw_abort_signal.clone();
        Arc::new(move |signal: CancellationToken| {
            let saw_abort_signal = saw_abort_signal.clone();
            Box::pin(async move {
                // The signal is the active run's abort token (not cancelled).
                saw_abort_signal.store(!signal.is_cancelled(), Ordering::SeqCst);
                None
            })
        })
    });
    let agent = Agent::new(options);

    tokio::time::timeout(TEST_TIMEOUT, agent.prompt("start"))
        .await
        .expect("prompt must not hang")
        .expect("prompt resolves");

    assert_eq!(provider.call_count(), 2);
    assert!(saw_abort_signal.load(Ordering::SeqCst));
}

#[tokio::test]
async fn forwards_session_id_to_stream_fn_options() {
    let received: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let stream_fn: StreamFn = {
        let received = received.clone();
        Arc::new(move |_model, _context, options| {
            received.lock().unwrap().push(options.session_id.clone());
            futures::stream::iter(vec![StreamEvent::Done {
                reason: pir_ai::types::DoneReason::Stop,
                message: assistant_text("ok"),
            }])
            .boxed()
        })
    };
    let mut options = AgentOptions::new(stream_fn);
    options.session_id = Some("session-abc".to_owned());
    let mut agent = Agent::new(options);

    tokio::time::timeout(TEST_TIMEOUT, agent.prompt("hello"))
        .await
        .expect("prompt must not hang")
        .expect("prompt resolves");
    assert_eq!(
        received.lock().unwrap().last().cloned().flatten(),
        Some("session-abc".to_owned())
    );

    // Setter (pub field in Rust) takes effect for the next run.
    agent.session_id = Some("session-def".to_owned());
    assert_eq!(agent.session_id.as_deref(), Some("session-def"));
    tokio::time::timeout(TEST_TIMEOUT, agent.prompt("hello again"))
        .await
        .expect("prompt must not hang")
        .expect("prompt resolves");
    assert_eq!(
        received.lock().unwrap().last().cloned().flatten(),
        Some("session-def".to_owned())
    );
}

#[tokio::test]
async fn forwards_thinking_level_to_stream_fn_options() {
    // Regression: the thinking level set on the agent (via /settings, the
    // thinking selector or setThinkingLevel) must reach the request through
    // `StreamOptions.reasoning` (design §4.4 D-013; upstream spreads the
    // whole config into the stream options). Before the fix the level only
    // sat on `AgentLoopConfig.reasoning`, which the pinned `StreamFn` shape
    // never forwards, so requests went out with thinking disabled.
    let received: Arc<Mutex<Vec<Option<pir_ai::types::ModelThinkingLevel>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let stream_fn: StreamFn = {
        let received = received.clone();
        Arc::new(move |_model, _context, options| {
            received.lock().unwrap().push(options.reasoning);
            futures::stream::iter(vec![StreamEvent::Done {
                reason: pir_ai::types::DoneReason::Stop,
                message: assistant_text("ok"),
            }])
            .boxed()
        })
    };

    // Off (default) → None (thinking omitted / disabled per compat).
    let agent = Agent::new(AgentOptions::new(stream_fn.clone()));
    tokio::time::timeout(TEST_TIMEOUT, agent.prompt("hello"))
        .await
        .expect("prompt must not hang")
        .expect("prompt resolves");
    assert_eq!(received.lock().unwrap().last().copied().flatten(), None);

    // xhigh → Some(Xhigh).
    agent.set_thinking_level(pir_agent::types::ThinkingLevel::Xhigh);
    tokio::time::timeout(TEST_TIMEOUT, agent.prompt("think hard"))
        .await
        .expect("prompt must not hang")
        .expect("prompt resolves");
    assert_eq!(
        received.lock().unwrap().last().copied().flatten(),
        Some(pir_ai::types::ModelThinkingLevel::Xhigh)
    );
}
