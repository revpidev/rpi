//! Port of `external/pi/packages/agent/test/agent-loop.test.ts` @ 2efa728.
//!
//! The upstream `MockAssistantStream` pushes only a terminal `done` event (no
//! partials), so no `message_update` events appear in the asserted sequences;
//! `mock_stream_fn` below mirrors that exactly. The TS-only "default stream
//! function compatibility" case is intentionally not ported (Rust has no
//! ambient default stream fn; `StreamFn` is always injected).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::StreamExt;
use pir_agent::agent_loop::{
    agent_loop, agent_loop_continue, AgentContext, AgentEventStream, AgentLoopConfig,
    AgentLoopTurnUpdate, BeforeToolCallResult, ConvertToLlmFn,
};
use pir_agent::messages::{convert_to_llm, AgentMessage, CustomMessage, CustomRole};
use pir_agent::stream_fn::StreamFn;
use pir_agent::types::{
    AgentEvent, AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode,
};
use pir_agent::AgentError;
use pir_ai::types::{
    ApiKind, AssistantContent, AssistantMessage, Context, DoneReason, ErrorReason, InputModality,
    Message, Model, ModelCost, StopReason, StreamEvent, StreamOptions, TextContent,
    ToolResultContent, Usage, UserContent, UserMessage, UserRole,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn test_model() -> Model {
    Model {
        id: "mock".to_owned(),
        name: "mock".to_owned(),
        api: ApiKind::from("openai-responses"),
        provider: "openai".to_owned(),
        base_url: "https://example.invalid".to_owned(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 8192,
        max_tokens: 2048,
        headers: None,
        compat: None,
    }
}

/// Upstream `identityConverter`: keep user/assistant/toolResult (here via the
/// coding-agent `convertToLlm` port, identical for base message kinds).
fn identity_converter() -> ConvertToLlmFn {
    Arc::new(|messages: Vec<AgentMessage>| Box::pin(async move { convert_to_llm(&messages) }))
}

fn test_config() -> AgentLoopConfig {
    AgentLoopConfig {
        model: test_model(),
        reasoning: None,
        thinking_budgets: None,
        stream_options: StreamOptions::default(),
        tool_execution: ToolExecutionMode::Parallel,
        convert_to_llm: identity_converter(),
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        before_tool_call: None,
        after_tool_call: None,
    }
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Text(text.to_owned()),
        timestamp: 1,
    })
}

fn assistant_message(content: Vec<AssistantContent>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        role: pir_ai::types::AssistantRole::Assistant,
        content,
        api: ApiKind::from("openai-responses"),
        provider: "openai".to_owned(),
        model: "mock".to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason,
        error_message: None,
        timestamp: 2,
    }
}

fn text_assistant(text: &str) -> AssistantMessage {
    assistant_message(
        vec![AssistantContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        StopReason::Stop,
    )
}

fn tool_call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(pir_ai::types::ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.as_object().expect("arguments object").clone(),
        thought_signature: None,
    })
}

/// Recorded LLM call (the context/options the stream fn was invoked with).
struct RecordedCall {
    context: Context,
    options: StreamOptions,
}

struct MockScript {
    queue: VecDeque<AssistantMessage>,
    calls: Vec<RecordedCall>,
}

/// Upstream `MockAssistantStream`: each LLM call pops one scripted assistant
/// message and emits only the terminal `done`/`error` event for it.
fn mock_stream_fn(script: Vec<AssistantMessage>) -> (StreamFn, Arc<Mutex<MockScript>>) {
    let state = Arc::new(Mutex::new(MockScript {
        queue: script.into(),
        calls: Vec::new(),
    }));
    let fn_state = state.clone();
    let stream_fn: StreamFn = Arc::new(move |_model, context, options| {
        let message = {
            let mut state = fn_state.lock().unwrap();
            state.calls.push(RecordedCall { context, options });
            state.queue.pop_front().expect("mock script exhausted")
        };
        let event = match message.stop_reason {
            StopReason::Error => StreamEvent::Error {
                reason: ErrorReason::Error,
                error: message,
            },
            StopReason::Aborted => StreamEvent::Error {
                reason: ErrorReason::Aborted,
                error: message,
            },
            StopReason::Stop => StreamEvent::Done {
                reason: DoneReason::Stop,
                message,
            },
            StopReason::Length => StreamEvent::Done {
                reason: DoneReason::Length,
                message,
            },
            StopReason::ToolUse => StreamEvent::Done {
                reason: DoneReason::ToolUse,
                message,
            },
            StopReason::Pending => panic!("scripted message needs a terminal stop reason"),
        };
        futures::stream::iter(vec![event]).boxed()
    });
    (stream_fn, state)
}

fn call_count(state: &Arc<Mutex<MockScript>>) -> usize {
    state.lock().unwrap().calls.len()
}

fn recorded_context(state: &Arc<Mutex<MockScript>>, index: usize) -> Context {
    state.lock().unwrap().calls[index].context.clone()
}

// ---------------------------------------------------------------------------
// Test tool
// ---------------------------------------------------------------------------

type ExecuteFn = Arc<
    dyn Fn(
            Value,
            Option<AgentToolUpdateCallback>,
        ) -> BoxFuture<'static, Result<AgentToolResult, AgentError>>
        + Send
        + Sync,
>;

struct TestTool {
    name: String,
    parameters: Value,
    mode: Option<ToolExecutionMode>,
    prepare: Option<Arc<dyn Fn(Value) -> Value + Send + Sync>>,
    execute_fn: ExecuteFn,
}

impl TestTool {
    fn new(name: &str, execute_fn: ExecuteFn) -> Self {
        Self {
            name: name.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"],
            }),
            mode: None,
            prepare: None,
            execute_fn,
        }
    }

    fn with_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.mode = Some(mode);
        self
    }

    fn with_prepare(mut self, prepare: impl Fn(Value) -> Value + Send + Sync + 'static) -> Self {
        self.prepare = Some(Arc::new(prepare));
        self
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
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.mode
    }
    fn prepare_arguments(&self, args: Value) -> Value {
        match &self.prepare {
            Some(prepare) => prepare(args),
            None => args,
        }
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, AgentError> {
        (self.execute_fn)(params, on_update).await
    }
}

fn ok_result(text: String) -> Result<AgentToolResult, AgentError> {
    Ok(AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text,
            text_signature: None,
        })],
        details: json!({}),
        ..Default::default()
    })
}

/// Echo tool recording the `value` argument of every execution.
fn echo_tool(executed: Arc<Mutex<Vec<Value>>>) -> TestTool {
    TestTool::new(
        "echo",
        Arc::new(move |params, _on_update| {
            let executed = executed.clone();
            Box::pin(async move {
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                executed.lock().unwrap().push(value.clone());
                ok_result(format!("echoed: {value}"))
            })
        }),
    )
}

// ---------------------------------------------------------------------------
// Event collection
// ---------------------------------------------------------------------------

async fn collect(stream: AgentEventStream) -> (Vec<AgentEvent>, Vec<AgentMessage>) {
    let result_handle = stream.clone();
    let events = stream.collect::<Vec<_>>().await;
    let messages = result_handle.result().await;
    (events, messages)
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

fn event_types(events: &[AgentEvent]) -> Vec<&'static str> {
    events.iter().map(event_type).collect()
}

fn message_roles(messages: &[AgentMessage]) -> Vec<&'static str> {
    messages
        .iter()
        .map(|m| match m {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolResult",
            AgentMessage::BashExecution(_) => "bashExecution",
            AgentMessage::Custom(_) => "custom",
            AgentMessage::BranchSummary(_) => "branchSummary",
            AgentMessage::CompactionSummary(_) => "compactionSummary",
        })
        .collect()
}

/// `message_start` markers in emission order: `tool:<id>` for tool results,
/// the text for string-content user messages (upstream test shape).
fn message_start_markers(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            let AgentEvent::MessageStart { message } = event else {
                return None;
            };
            match message {
                AgentMessage::ToolResult(t) => Some(format!("tool:{}", t.tool_call_id)),
                AgentMessage::User(u) => match &u.content {
                    UserContent::Text(text) => Some(text.clone()),
                    UserContent::Blocks(_) => None,
                },
                _ => None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// agentLoop with AgentMessage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn emits_events_with_agent_message_types() {
    let context = AgentContext {
        system_prompt: "You are helpful.".to_owned(),
        messages: Vec::new(),
        tools: None,
    };
    let (stream_fn, _state) = mock_stream_fn(vec![text_assistant("Hi there!")]);
    let stream = agent_loop(
        vec![user_message("Hello")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    let (events, messages) = collect(stream).await;

    assert_eq!(messages.len(), 2);
    assert_eq!(message_roles(&messages), ["user", "assistant"]);
    assert_eq!(
        event_types(&events),
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
}

#[tokio::test]
async fn handles_custom_message_types_via_convert_to_llm() {
    let notification = AgentMessage::Custom(CustomMessage {
        role: CustomRole::Custom,
        custom_type: "notification".to_owned(),
        content: UserContent::Text("This is a notification".to_owned()),
        display: true,
        details: None,
        timestamp: 5,
    });
    let context = AgentContext {
        system_prompt: "You are helpful.".to_owned(),
        messages: vec![notification],
        tools: None,
    };

    // Upstream filters the custom role out inside convertToLlm.
    let converted: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let converted_clone = converted.clone();
    let mut config = test_config();
    config.convert_to_llm = Arc::new(move |messages: Vec<AgentMessage>| {
        let converted = converted_clone.clone();
        Box::pin(async move {
            let llm: Vec<Message> = messages
                .into_iter()
                .filter(|m| !matches!(m, AgentMessage::Custom(_)))
                .filter_map(|m| match m {
                    AgentMessage::User(u) => Some(Message::User(u)),
                    AgentMessage::Assistant(a) => Some(Message::Assistant(a)),
                    AgentMessage::ToolResult(t) => Some(Message::ToolResult(t)),
                    _ => None,
                })
                .collect();
            *converted.lock().unwrap() = llm.clone();
            llm
        })
    });

    let (stream_fn, _state) = mock_stream_fn(vec![text_assistant("Response")]);
    let stream = agent_loop(
        vec![user_message("Hello")],
        context,
        config,
        None,
        stream_fn,
    );
    let (_events, _messages) = collect(stream).await;

    // Only the prompt user message survives conversion.
    let converted = converted.lock().unwrap();
    assert_eq!(converted.len(), 1);
    assert!(matches!(converted[0], Message::User(_)));
}

#[tokio::test]
async fn applies_transform_context_before_convert_to_llm() {
    let context = AgentContext {
        system_prompt: "You are helpful.".to_owned(),
        messages: vec![
            user_message("old message 1"),
            AgentMessage::Assistant(text_assistant("old response 1")),
            user_message("old message 2"),
            AgentMessage::Assistant(text_assistant("old response 2")),
        ],
        tools: None,
    };

    let transformed_len = Arc::new(Mutex::new(0usize));
    let converted_len = Arc::new(Mutex::new(0usize));
    let mut config = test_config();
    config.transform_context = Some({
        let transformed_len = transformed_len.clone();
        Arc::new(move |messages: Vec<AgentMessage>, _signal| {
            let transformed_len = transformed_len.clone();
            Box::pin(async move {
                // Prune: keep only the last 2 messages.
                let pruned: Vec<AgentMessage> = messages
                    .iter()
                    .skip(messages.len().saturating_sub(2))
                    .cloned()
                    .collect();
                *transformed_len.lock().unwrap() = pruned.len();
                pruned
            })
        })
    });
    config.convert_to_llm = {
        let converted_len = converted_len.clone();
        Arc::new(move |messages: Vec<AgentMessage>| {
            let converted_len = converted_len.clone();
            Box::pin(async move {
                *converted_len.lock().unwrap() = messages.len();
                convert_to_llm(&messages)
            })
        })
    };

    let (stream_fn, _state) = mock_stream_fn(vec![text_assistant("Response")]);
    let stream = agent_loop(
        vec![user_message("new message")],
        context,
        config,
        None,
        stream_fn,
    );
    let (_events, _messages) = collect(stream).await;

    // transformContext ran first (5 -> 2), convertToLlm saw the pruned list.
    assert_eq!(*transformed_len.lock().unwrap(), 2);
    assert_eq!(*converted_len.lock().unwrap(), 2);
}

#[tokio::test]
async fn handles_tool_calls_and_results() {
    let tool_usage = Usage {
        input: 1,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        total_tokens: 10,
        ..Default::default()
    };
    let patched_tool_usage = Usage {
        input: 5,
        output: 6,
        cache_read: 7,
        cache_write: 8,
        total_tokens: 26,
        ..Default::default()
    };

    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = TestTool::new(
        "echo",
        Arc::new({
            let executed = executed.clone();
            let tool_usage = tool_usage.clone();
            move |params, _on_update| {
                let executed = executed.clone();
                let tool_usage = tool_usage.clone();
                Box::pin(async move {
                    let value = params.get("value").cloned().unwrap_or(Value::Null);
                    executed.lock().unwrap().push(value.clone());
                    Ok(AgentToolResult {
                        content: vec![ToolResultContent::Text(TextContent {
                            text: format!("echoed: {value}"),
                            text_signature: None,
                        })],
                        details: json!({ "value": value }),
                        usage: Some(tool_usage),
                        ..Default::default()
                    })
                })
            }
        }),
    );

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let observed_usage: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
    let mut config = test_config();
    config.after_tool_call = Some({
        let observed_usage = observed_usage.clone();
        let patched = patched_tool_usage.clone();
        Arc::new(move |hook_context, _signal| {
            let observed_usage = observed_usage.clone();
            let patched = patched.clone();
            Box::pin(async move {
                *observed_usage.lock().unwrap() = hook_context.result.usage.clone();
                Ok(Some(pir_agent::agent_loop::AfterToolCallResult {
                    usage: Some(patched),
                    ..Default::default()
                }))
            })
        })
    });

    let (stream_fn, _state) = mock_stream_fn(vec![
        assistant_message(
            vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        config,
        None,
        stream_fn,
    );
    let (events, messages) = collect(stream).await;

    assert_eq!(*executed.lock().unwrap(), vec![json!("hello")]);

    let tool_start = events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }));
    assert!(tool_start.is_some());
    let tool_end = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            is_error,
            ..
        } => Some((tool_call_id.clone(), *is_error)),
        _ => None,
    });
    assert_eq!(tool_end, Some(("tool-1".to_owned(), false)));

    // afterToolCall saw the raw execution usage; the patch reached the
    // persisted toolResult message.
    assert_eq!(*observed_usage.lock().unwrap(), Some(tool_usage));
    let tool_result = messages.iter().find_map(|m| match m {
        AgentMessage::ToolResult(t) => Some(t),
        _ => None,
    });
    assert_eq!(
        tool_result.and_then(|t| t.usage.clone()),
        Some(patched_tool_usage)
    );
}

#[tokio::test]
async fn does_not_execute_tool_calls_from_length_truncated_message() {
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let (stream_fn, state) = mock_stream_fn(vec![
        // Output hit the token limit mid tool call: the salvaged arguments
        // may be silently truncated, so nothing in this message may execute.
        assistant_message(
            vec![tool_call("tool-1", "echo", json!({ "value": "hel" }))],
            StopReason::Length,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    let (events, messages) = collect(stream).await;

    assert!(executed.lock().unwrap().is_empty());

    let (is_error, result_text) = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                is_error, result, ..
            } => {
                let text = result
                    .get("content")
                    .and_then(Value::as_array)
                    .and_then(|content| content.first())
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Some((*is_error, text))
            }
            _ => None,
        })
        .expect("tool_execution_end emitted");
    assert!(is_error);
    assert!(result_text.contains("output token limit"));

    // The loop continues so the model can re-issue the tool call.
    assert_eq!(call_count(&state), 2);
    assert_eq!(message_roles(&messages).last().copied(), Some("assistant"));
}

#[tokio::test]
async fn executes_mutated_before_tool_call_args_without_revalidation() {
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let mut config = test_config();
    config.before_tool_call = Some(Arc::new(|_hook_context, _signal| {
        Box::pin(async move {
            // Upstream mutates the validated args object in place
            // (`args.value = 123`); the mutation must reach execute without
            // being revalidated against the string schema.
            Some(BeforeToolCallResult {
                args: Some(json!({ "value": 123 })),
                ..Default::default()
            })
        })
    }));

    let (stream_fn, _state) = mock_stream_fn(vec![
        assistant_message(
            vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        config,
        None,
        stream_fn,
    );
    let (_events, _messages) = collect(stream).await;

    assert_eq!(*executed.lock().unwrap(), vec![json!(123)]);
}

#[tokio::test]
async fn prepares_tool_arguments_for_validation() {
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let mut tool = TestTool::new(
        "edit",
        Arc::new({
            let executed = executed.clone();
            move |params, _on_update| {
                let executed = executed.clone();
                Box::pin(async move {
                    let edits = params.get("edits").cloned().unwrap_or(Value::Null);
                    executed.lock().unwrap().push(edits.clone());
                    let count = edits.as_array().map_or(0, Vec::len);
                    Ok(AgentToolResult {
                        content: vec![ToolResultContent::Text(TextContent {
                            text: format!("edited {count}"),
                            text_signature: None,
                        })],
                        details: json!({ "count": count }),
                        ..Default::default()
                    })
                })
            }
        }),
    );
    tool.parameters = json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldText": { "type": "string" },
                        "newText": { "type": "string" },
                    },
                    "required": ["oldText", "newText"],
                },
            },
        },
        "required": ["edits"],
    });
    // Shim: flat {oldText, newText} -> {edits: [...]}, before schema validation.
    let tool = tool.with_prepare(|args| {
        let Some(obj) = args.as_object() else {
            return args;
        };
        let (Some(old), Some(new)) = (obj.get("oldText"), obj.get("newText")) else {
            return args;
        };
        if !(old.is_string() && new.is_string()) {
            return args;
        }
        let mut edits = obj
            .get("edits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        edits.push(json!({ "oldText": old, "newText": new }));
        json!({ "edits": edits })
    });

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let (stream_fn, _state) = mock_stream_fn(vec![
        assistant_message(
            vec![tool_call(
                "tool-1",
                "edit",
                json!({ "oldText": "before", "newText": "after" }),
            )],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("edit something")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    let (_events, _messages) = collect(stream).await;

    assert_eq!(
        *executed.lock().unwrap(),
        vec![json!([{ "oldText": "before", "newText": "after" }])]
    );
}

// ---------------------------------------------------------------------------
// Parallel / sequential execution semantics
// ---------------------------------------------------------------------------

/// Barrier tool pair shared by the parallel/sequential tests: the "first"
/// call waits on a gate; the "second" call records whether the first one had
/// already finished (`parallel_observed`).
struct GateState {
    first_resolved: AtomicBool,
    parallel_observed: AtomicBool,
}

fn gated_echo_tool(name: &str, gate: Arc<tokio::sync::Notify>, state: Arc<GateState>) -> TestTool {
    TestTool::new(
        name,
        Arc::new(move |params, _on_update| {
            let gate = gate.clone();
            let state = state.clone();
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if value == "first" {
                    gate.notified().await;
                    state.first_resolved.store(true, Ordering::SeqCst);
                }
                if value == "second" && !state.first_resolved.load(Ordering::SeqCst) {
                    state.parallel_observed.store(true, Ordering::SeqCst);
                }
                ok_result(format!("echoed: {value}"))
            })
        }),
    )
}

fn two_call_script(tool_name: &str) -> Vec<AssistantMessage> {
    vec![
        assistant_message(
            vec![
                tool_call("tool-1", tool_name, json!({ "value": "first" })),
                tool_call("tool-2", tool_name, json!({ "value": "second" })),
            ],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]
}

fn tool_execution_end_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_message_end_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(t),
            } => Some(t.tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

fn turn_end_tool_result_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .flat_map(|e| match e {
            AgentEvent::TurnEnd { tool_results, .. } => tool_results
                .iter()
                .map(|t| t.tool_call_id.clone())
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

#[tokio::test]
async fn emits_tool_execution_end_in_completion_order_results_in_source_order() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let gate_state = Arc::new(GateState {
        first_resolved: AtomicBool::new(false),
        parallel_observed: AtomicBool::new(false),
    });
    let tool = gated_echo_tool("echo", gate.clone(), gate_state.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let mut config = test_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let (stream_fn, _state) = mock_stream_fn(two_call_script("echo"));
    let stream = agent_loop(
        vec![user_message("echo both")],
        context,
        config,
        None,
        stream_fn,
    );
    // Release the first tool shortly after (upstream setTimeout(20)).
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.notify_one();
    });

    let (events, _messages) = tokio::time::timeout(Duration::from_secs(5), collect(stream))
        .await
        .expect("loop must not hang");

    assert!(gate_state.parallel_observed.load(Ordering::SeqCst));
    assert_eq!(tool_execution_end_ids(&events), ["tool-2", "tool-1"]);
    assert_eq!(tool_result_message_end_ids(&events), ["tool-1", "tool-2"]);
    assert_eq!(turn_end_tool_result_ids(&events), ["tool-1", "tool-2"]);
}

#[tokio::test]
async fn injects_queued_messages_after_all_tool_calls_complete() {
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let queued_delivered = Arc::new(AtomicBool::new(false));
    let mut config = test_config();
    config.tool_execution = ToolExecutionMode::Sequential;
    config.get_steering_messages = Some({
        let executed = executed.clone();
        let queued_delivered = queued_delivered.clone();
        Arc::new(move || {
            let executed = executed.clone();
            let queued_delivered = queued_delivered.clone();
            Box::pin(async move {
                // Deliver the steering message once tool execution started.
                if !executed.lock().unwrap().is_empty()
                    && !queued_delivered.swap(true, Ordering::SeqCst)
                {
                    return vec![user_message("interrupt")];
                }
                Vec::new()
            })
        })
    });

    let (stream_fn, state) = mock_stream_fn(two_call_script("echo"));
    let stream = agent_loop(
        vec![user_message("start")],
        context,
        config,
        None,
        stream_fn,
    );
    let (events, _messages) = tokio::time::timeout(Duration::from_secs(5), collect(stream))
        .await
        .expect("loop must not hang");

    // Both tools executed before the steering message was injected.
    assert_eq!(
        *executed.lock().unwrap(),
        vec![json!("first"), json!("second")]
    );

    let tool_ends: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .collect();
    assert_eq!(tool_ends, [false, false]);

    let markers = message_start_markers(&events);
    assert!(markers.contains(&"interrupt".to_owned()));
    let pos = |marker: &str| markers.iter().position(|m| m == marker).unwrap();
    assert!(pos("tool:tool-1") < pos("interrupt"));
    assert!(pos("tool:tool-2") < pos("interrupt"));

    // The injected message is in the context of the second LLM call.
    let second_context = recorded_context(&state, 1);
    let saw_interrupt = second_context.messages.iter().any(|m| match m {
        Message::User(u) => matches!(&u.content, UserContent::Text(t) if t == "interrupt"),
        _ => false,
    });
    assert!(saw_interrupt);
}

#[tokio::test]
async fn forces_sequential_when_tool_has_execution_mode_sequential() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let gate_state = Arc::new(GateState {
        first_resolved: AtomicBool::new(false),
        parallel_observed: AtomicBool::new(false),
    });
    let tool = gated_echo_tool("slow", gate.clone(), gate_state.clone())
        .with_mode(ToolExecutionMode::Sequential);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    // Config is parallel (default), but the tool forces sequential.
    let (stream_fn, _state) = mock_stream_fn(two_call_script("slow"));
    let stream = agent_loop(
        vec![user_message("run both")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.notify_one();
    });

    let (events, _messages) = tokio::time::timeout(Duration::from_secs(5), collect(stream))
        .await
        .expect("loop must not hang");

    // Sequential: the second tool must not start before the first finishes.
    assert!(!gate_state.parallel_observed.load(Ordering::SeqCst));
    assert_eq!(tool_result_message_end_ids(&events), ["tool-1", "tool-2"]);
}

#[tokio::test]
async fn forces_sequential_when_one_of_multiple_tools_is_sequential() {
    let execution_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let slow_gate = Arc::new(tokio::sync::Notify::new());

    let slow_tool = TestTool::new(
        "slow",
        Arc::new({
            let execution_order = execution_order.clone();
            let slow_gate = slow_gate.clone();
            move |params, _on_update| {
                let execution_order = execution_order.clone();
                let slow_gate = slow_gate.clone();
                Box::pin(async move {
                    let value = params
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    execution_order
                        .lock()
                        .unwrap()
                        .push(format!("slow:{value}"));
                    if value == "a" {
                        slow_gate.notified().await;
                    }
                    ok_result(format!("slow: {value}"))
                })
            }
        }),
    )
    .with_mode(ToolExecutionMode::Sequential);
    let fast_tool = TestTool::new(
        "fast",
        Arc::new({
            let execution_order = execution_order.clone();
            move |params, _on_update| {
                let execution_order = execution_order.clone();
                Box::pin(async move {
                    let value = params
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    execution_order
                        .lock()
                        .unwrap()
                        .push(format!("fast:{value}"));
                    ok_result(format!("fast: {value}"))
                })
            }
        }),
    );

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(slow_tool), Arc::new(fast_tool)]),
    };

    let (stream_fn, _state) = mock_stream_fn(vec![
        assistant_message(
            vec![
                tool_call("tool-1", "slow", json!({ "value": "a" })),
                tool_call("tool-2", "fast", json!({ "value": "b" })),
            ],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    // Parallel by default, but the slow tool forces sequential.
    let stream = agent_loop(
        vec![user_message("run both")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        slow_gate.notify_one();
    });

    let (_events, _messages) = tokio::time::timeout(Duration::from_secs(5), collect(stream))
        .await
        .expect("loop must not hang");

    // Fast tool must not run before the slow tool finishes.
    let order = execution_order.lock().unwrap();
    assert_eq!(order.first().map(String::as_str), Some("slow:a"));
    assert!(order.contains(&"fast:b".to_owned()));
}

#[tokio::test]
async fn allows_parallel_when_all_tools_have_execution_mode_parallel() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let gate_state = Arc::new(GateState {
        first_resolved: AtomicBool::new(false),
        parallel_observed: AtomicBool::new(false),
    });
    let tool = gated_echo_tool("echo", gate.clone(), gate_state.clone())
        .with_mode(ToolExecutionMode::Parallel);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let (stream_fn, _state) = mock_stream_fn(two_call_script("echo"));
    let stream = agent_loop(
        vec![user_message("echo both")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.notify_one();
    });

    let (_events, _messages) = tokio::time::timeout(Duration::from_secs(5), collect(stream))
        .await
        .expect("loop must not hang");

    assert!(gate_state.parallel_observed.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// Turn-level hooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uses_prepare_next_turn_snapshot_before_continuing() {
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed);
    let context = AgentContext {
        system_prompt: "first prompt".to_owned(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let prepared = Arc::new(AtomicBool::new(false));
    let mut config = test_config();
    config.prepare_next_turn = Some({
        let prepared = prepared.clone();
        Arc::new(move |hook_context| {
            let prepared = prepared.clone();
            Box::pin(async move {
                if prepared.swap(true, Ordering::SeqCst) {
                    return None;
                }
                Some(AgentLoopTurnUpdate {
                    context: Some(AgentContext {
                        system_prompt: "second prompt".to_owned(),
                        messages: hook_context.context.messages.clone(),
                        tools: hook_context.context.tools.clone(),
                    }),
                    model: None,
                    thinking_level: None,
                })
            })
        })
    });

    let (stream_fn, state) = mock_stream_fn(vec![
        assistant_message(
            vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        config,
        None,
        stream_fn,
    );
    let (_events, _messages) = collect(stream).await;

    assert_eq!(call_count(&state), 2);
    assert_eq!(
        recorded_context(&state, 1).system_prompt.as_deref(),
        Some("second prompt")
    );
}

#[tokio::test]
async fn stops_after_turn_when_should_stop_after_turn_returns_true() {
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let steering_polls = Arc::new(Mutex::new(0u32));
    let follow_up_polls = Arc::new(Mutex::new(0u32));
    let callback_tool_result_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let callback_context_roles: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let mut config = test_config();
    config.get_steering_messages = Some({
        let steering_polls = steering_polls.clone();
        Arc::new(move || {
            let steering_polls = steering_polls.clone();
            Box::pin(async move {
                *steering_polls.lock().unwrap() += 1;
                Vec::new()
            })
        })
    });
    config.get_follow_up_messages = Some({
        let follow_up_polls = follow_up_polls.clone();
        Arc::new(move || {
            let follow_up_polls = follow_up_polls.clone();
            Box::pin(async move {
                *follow_up_polls.lock().unwrap() += 1;
                vec![user_message("follow up should stay queued")]
            })
        })
    });
    config.should_stop_after_turn = Some({
        let callback_tool_result_ids = callback_tool_result_ids.clone();
        let callback_context_roles = callback_context_roles.clone();
        Arc::new(move |hook_context| {
            let callback_tool_result_ids = callback_tool_result_ids.clone();
            let callback_context_roles = callback_context_roles.clone();
            Box::pin(async move {
                assert!(matches!(hook_context.message, AssistantMessage { .. }));
                *callback_tool_result_ids.lock().unwrap() = hook_context
                    .tool_results
                    .iter()
                    .map(|t| t.tool_call_id.clone())
                    .collect();
                *callback_context_roles.lock().unwrap() =
                    message_roles(&hook_context.context.messages)
                        .iter()
                        .map(|r| (*r).to_owned())
                        .collect();
                true
            })
        })
    });

    let (stream_fn, state) = mock_stream_fn(vec![
        assistant_message(
            vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_assistant("should not run"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        config,
        None,
        stream_fn,
    );
    let (events, messages) = collect(stream).await;

    assert_eq!(call_count(&state), 1);
    assert_eq!(*executed.lock().unwrap(), vec![json!("hello")]);
    assert_eq!(*steering_polls.lock().unwrap(), 1);
    assert_eq!(*follow_up_polls.lock().unwrap(), 0);
    assert_eq!(
        *callback_tool_result_ids.lock().unwrap(),
        vec!["tool-1".to_owned()]
    );
    assert_eq!(
        *callback_context_roles.lock().unwrap(),
        vec!["user", "assistant", "toolResult"]
    );
    assert_eq!(
        message_roles(&messages),
        ["user", "assistant", "toolResult"]
    );
    assert_eq!(
        event_types(&events),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
}

#[tokio::test]
async fn stops_after_tool_batch_when_every_tool_result_terminates() {
    let tool = TestTool::new(
        "echo",
        Arc::new(|params, _on_update| {
            Box::pin(async move {
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent {
                        text: format!("echoed: {value}"),
                        text_signature: None,
                    })],
                    details: json!({ "value": value }),
                    terminate: Some(true),
                    ..Default::default()
                })
            })
        }),
    );
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let (stream_fn, state) = mock_stream_fn(vec![assistant_message(
        vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
        StopReason::ToolUse,
    )]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    let (events, messages) = collect(stream).await;

    assert_eq!(call_count(&state), 1);
    assert_eq!(
        message_roles(&messages),
        ["user", "assistant", "toolResult"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn continues_after_parallel_tool_calls_when_not_all_terminate() {
    let tool = TestTool::new(
        "echo",
        Arc::new(|params, _on_update| {
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent {
                        text: format!("echoed: {value}"),
                        text_signature: None,
                    })],
                    details: json!({ "value": value }),
                    terminate: Some(value == "first"),
                    ..Default::default()
                })
            })
        }),
    );
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let mut config = test_config();
    config.tool_execution = ToolExecutionMode::Parallel;
    let (stream_fn, state) = mock_stream_fn(two_call_script("echo"));
    let stream = agent_loop(
        vec![user_message("echo both")],
        context,
        config,
        None,
        stream_fn,
    );
    let (_events, messages) = collect(stream).await;

    assert_eq!(call_count(&state), 2);
    assert_eq!(
        message_roles(&messages),
        ["user", "assistant", "toolResult", "toolResult", "assistant"]
    );
}

#[tokio::test]
async fn allows_after_tool_call_to_mark_batch_as_terminating() {
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let mut config = test_config();
    config.after_tool_call = Some(Arc::new(|_hook_context, _signal| {
        Box::pin(async move {
            Ok(Some(pir_agent::agent_loop::AfterToolCallResult {
                terminate: Some(true),
                ..Default::default()
            }))
        })
    }));

    let (stream_fn, state) = mock_stream_fn(vec![assistant_message(
        vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
        StopReason::ToolUse,
    )]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        config,
        None,
        stream_fn,
    );
    let (_events, _messages) = collect(stream).await;

    assert_eq!(call_count(&state), 1);
}

// ---------------------------------------------------------------------------
// agentLoopContinue with AgentMessage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_loop_continue_throws_when_context_has_no_messages() {
    let context = AgentContext {
        system_prompt: "You are helpful.".to_owned(),
        messages: Vec::new(),
        tools: None,
    };
    let (stream_fn, _state) = mock_stream_fn(vec![]);
    let result = agent_loop_continue(context, test_config(), None, stream_fn);
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("must fail validation"),
    };
    assert_eq!(error.to_string(), "Cannot continue: no messages in context");
}

#[tokio::test]
async fn agent_loop_continue_throws_when_last_message_is_assistant() {
    let context = AgentContext {
        system_prompt: "You are helpful.".to_owned(),
        messages: vec![
            user_message("Hello"),
            AgentMessage::Assistant(text_assistant("Hi")),
        ],
        tools: None,
    };
    let (stream_fn, _state) = mock_stream_fn(vec![]);
    let result = agent_loop_continue(context, test_config(), None, stream_fn);
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("must fail validation"),
    };
    assert_eq!(
        error.to_string(),
        "Cannot continue from message role: assistant"
    );
}

#[tokio::test]
async fn agent_loop_continue_from_existing_context_without_user_message_events() {
    let context = AgentContext {
        system_prompt: "You are helpful.".to_owned(),
        messages: vec![user_message("Hello")],
        tools: None,
    };
    let (stream_fn, _state) = mock_stream_fn(vec![text_assistant("Response")]);
    let stream =
        agent_loop_continue(context, test_config(), None, stream_fn).expect("validation passes");
    let (events, messages) = collect(stream).await;

    // Only the new assistant message is returned (not the existing user one).
    assert_eq!(messages.len(), 1);
    assert_eq!(message_roles(&messages), ["assistant"]);

    // No message events for the pre-existing user message.
    let message_end_roles: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd { message } => Some(message),
            _ => None,
        })
        .map(|m| match m {
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::User(_) => "user",
            _ => "other",
        })
        .collect();
    assert_eq!(message_end_roles, ["assistant"]);
}

#[tokio::test]
async fn agent_loop_continue_allows_custom_message_as_last_message() {
    let custom = AgentMessage::Custom(CustomMessage {
        role: CustomRole::Custom,
        custom_type: "hook".to_owned(),
        content: UserContent::Text("Hook content".to_owned()),
        display: true,
        details: None,
        timestamp: 5,
    });
    let context = AgentContext {
        system_prompt: "You are helpful.".to_owned(),
        messages: vec![custom],
        tools: None,
    };

    // The real convertToLlm maps custom messages to user messages.
    let (stream_fn, state) = mock_stream_fn(vec![text_assistant("Response to custom message")]);
    let stream =
        agent_loop_continue(context, test_config(), None, stream_fn).expect("validation passes");
    let (_events, messages) = collect(stream).await;

    assert_eq!(messages.len(), 1);
    assert_eq!(message_roles(&messages), ["assistant"]);
    let llm_context = recorded_context(&state, 0);
    assert!(llm_context
        .messages
        .iter()
        .any(|m| matches!(m, Message::User(u) if matches!(&u.content, UserContent::Blocks(_)))));
}

// ---------------------------------------------------------------------------
// API key resolution / abort inside tool preflight (requirements §4.3 anchors;
// agent-loop.ts:305-306 and agent-loop.ts:629-650/478-480)
// ---------------------------------------------------------------------------

fn recorded_api_key(state: &Arc<Mutex<MockScript>>, index: usize) -> Option<String> {
    state.lock().unwrap().calls[index].options.api_key.clone()
}

#[tokio::test]
async fn resolves_api_key_dynamically_before_each_llm_call() {
    // `getApiKey` is re-invoked before every LLM request and wins over the
    // static `config.apiKey` (agent-loop.ts:305-306).
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let key_calls = Arc::new(AtomicU64::new(0));
    let mut config = test_config();
    config.stream_options.api_key = Some("static-key".to_owned());
    config.get_api_key = Some({
        let key_calls = key_calls.clone();
        Arc::new(move |_provider: String| {
            let key_calls = key_calls.clone();
            Box::pin(async move {
                let n = key_calls.fetch_add(1, Ordering::SeqCst);
                Some(format!("key-{}", n + 1))
            })
        })
    });

    let (stream_fn, state) = mock_stream_fn(vec![
        assistant_message(
            vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        config,
        None,
        stream_fn,
    );
    let (_events, _messages) = collect(stream).await;

    assert_eq!(call_count(&state), 2);
    assert_eq!(key_calls.load(Ordering::SeqCst), 2);
    assert_eq!(recorded_api_key(&state, 0).as_deref(), Some("key-1"));
    assert_eq!(recorded_api_key(&state, 1).as_deref(), Some("key-2"));

    // Fallback branch: hook returning `None` — or an empty string, which
    // upstream `||` treats as missing — falls back to the static config key.
    for hook_result in [None, Some(String::new())] {
        let mut config = test_config();
        config.stream_options.api_key = Some("static-key".to_owned());
        config.get_api_key = Some({
            let hook_result = hook_result.clone();
            Arc::new(move |_provider: String| {
                let hook_result = hook_result.clone();
                Box::pin(async move { hook_result })
            })
        });

        let (stream_fn, state) = mock_stream_fn(vec![text_assistant("done")]);
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: None,
        };
        let stream = agent_loop(
            vec![user_message("hello")],
            context,
            config,
            None,
            stream_fn,
        );
        let (_events, _messages) = collect(stream).await;

        assert_eq!(call_count(&state), 1);
        assert_eq!(recorded_api_key(&state, 0).as_deref(), Some("static-key"));
    }
}

#[tokio::test]
async fn abort_in_before_tool_call_yields_operation_aborted_error_result() {
    // Preflight abort checks (agent-loop.ts:629-635, 644-650): an abort
    // observed inside `beforeToolCall` short-circuits to an immediate error
    // result with the verbatim text "Operation aborted"; the tool never
    // executes. In a sequential batch the remaining calls are skipped too
    // (agent-loop.ts:478-480 break).
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let mut config = test_config();
    config.tool_execution = ToolExecutionMode::Sequential;
    config.before_tool_call = Some(Arc::new(|_hook_context, signal: CancellationToken| {
        Box::pin(async move {
            // The hook receives the run's abort token; cancelling it here
            // simulates an abort observed mid-preflight.
            signal.cancel();
            None
        })
    }));

    let (stream_fn, state) = mock_stream_fn(vec![
        assistant_message(
            vec![
                tool_call("tool-1", "echo", json!({ "value": "first" })),
                tool_call("tool-2", "echo", json!({ "value": "second" })),
            ],
            StopReason::ToolUse,
        ),
        // The loop's next LLM call answers with an aborted message so the run
        // terminates (upstream behavior once the signal is cancelled).
        assistant_message(Vec::new(), StopReason::Aborted),
    ]);
    let signal = CancellationToken::new();
    let stream = agent_loop(
        vec![user_message("echo both")],
        context,
        config,
        Some(signal),
        stream_fn,
    );
    let (events, messages) = tokio::time::timeout(Duration::from_secs(5), collect(stream))
        .await
        .expect("loop must not hang");

    // The tool never executed.
    assert!(executed.lock().unwrap().is_empty());

    // The first call short-circuited with the verbatim abort text...
    let (is_error, result_text) = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                is_error, result, ..
            } => {
                let text = result
                    .get("content")
                    .and_then(Value::as_array)
                    .and_then(|content| content.first())
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Some((*is_error, text))
            }
            _ => None,
        })
        .expect("tool_execution_end emitted");
    assert!(is_error);
    assert_eq!(result_text, "Operation aborted");

    // ...and the sequential batch broke after it: the second call never even
    // emitted tool_execution_start, and only one tool result was produced.
    let start_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(start_ids, ["tool-1"]);
    assert_eq!(turn_end_tool_result_ids(&events), ["tool-1"]);
    let tool_results: Vec<&pir_ai::types::ToolResultMessage> = messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert!(tool_results[0].is_error);

    // The aborted second LLM response ended the run.
    assert_eq!(call_count(&state), 2);
    let AgentMessage::Assistant(last) = messages.last().expect("assistant tail") else {
        panic!("expected assistant tail");
    };
    assert_eq!(last.stop_reason, StopReason::Aborted);
}

// ---------------------------------------------------------------------------
// Tool preflight / hook failure anchors (requirements §4.3 items 4 and 6;
// agent-loop.ts:608-614, 636-641, 657-663, 743-746)
// ---------------------------------------------------------------------------

/// `(is_error, first content text)` of the first `tool_execution_end` event.
fn first_tool_execution_end_outcome(events: &[AgentEvent]) -> (bool, String) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                is_error, result, ..
            } => {
                let text = result
                    .get("content")
                    .and_then(Value::as_array)
                    .and_then(|content| content.first())
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Some((*is_error, text))
            }
            _ => None,
        })
        .expect("tool_execution_end emitted")
}

#[tokio::test]
async fn before_tool_call_block_yields_error_result_without_executing() {
    // `block: true` prevents execution; the loop emits an error tool result
    // and continues (agent-loop.ts:636-641). Two variants: the default
    // blocked message and a custom reason.
    for (hook_result, expected_text) in [
        (
            BeforeToolCallResult {
                block: Some(true),
                ..Default::default()
            },
            "Tool execution was blocked".to_owned(),
        ),
        (
            BeforeToolCallResult {
                block: Some(true),
                reason: Some("nope: dangerous command".to_owned()),
                ..Default::default()
            },
            "nope: dangerous command".to_owned(),
        ),
    ] {
        let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let tool = echo_tool(executed.clone());
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Some(vec![Arc::new(tool)]),
        };

        let mut config = test_config();
        config.before_tool_call = Some({
            let hook_result = hook_result.clone();
            Arc::new(move |_hook_context, _signal| {
                let hook_result = hook_result.clone();
                Box::pin(async move { Some(hook_result) })
            })
        });

        let (stream_fn, state) = mock_stream_fn(vec![
            assistant_message(
                vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
                StopReason::ToolUse,
            ),
            text_assistant("done"),
        ]);
        let stream = agent_loop(
            vec![user_message("echo something")],
            context,
            config,
            None,
            stream_fn,
        );
        let (events, messages) = collect(stream).await;

        assert!(executed.lock().unwrap().is_empty());
        let (is_error, result_text) = first_tool_execution_end_outcome(&events);
        assert!(is_error);
        assert_eq!(result_text, expected_text);

        // The error result reached the transcript and the loop continued.
        let tool_result = messages.iter().find_map(|m| match m {
            AgentMessage::ToolResult(t) => Some(t),
            _ => None,
        });
        assert_eq!(tool_result.map(|t| t.is_error), Some(true));
        assert_eq!(call_count(&state), 2);
        assert_eq!(message_roles(&messages).last().copied(), Some("assistant"));
    }
}

#[tokio::test]
async fn tool_not_found_yields_error_result_without_executing() {
    // Unknown tool name -> immediate error result with the verbatim upstream
    // text (agent-loop.ts:608-614); the loop is not interrupted.
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let (stream_fn, state) = mock_stream_fn(vec![
        assistant_message(
            vec![tool_call("tool-1", "ghost", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    let (events, messages) = collect(stream).await;

    assert!(executed.lock().unwrap().is_empty());
    let (is_error, result_text) = first_tool_execution_end_outcome(&events);
    assert!(is_error);
    assert_eq!(result_text, "Tool ghost not found");

    let tool_result = messages.iter().find_map(|m| match m {
        AgentMessage::ToolResult(t) => Some(t),
        _ => None,
    });
    assert_eq!(tool_result.map(|t| t.is_error), Some(true));
    assert_eq!(call_count(&state), 2);
    assert_eq!(message_roles(&messages).last().copied(), Some("assistant"));
}

#[tokio::test]
async fn validation_failure_yields_error_result_without_executing() {
    // Arguments failing schema validation become an error tool result
    // carrying the validation message (agent-loop.ts:657-663 catch-all);
    // execute never runs and the loop continues.
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let (stream_fn, state) = mock_stream_fn(vec![
        // Missing the required `value` property: coercion cannot synthesize
        // it, so schema validation must fail.
        assistant_message(
            vec![tool_call("tool-1", "echo", json!({}))],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        test_config(),
        None,
        stream_fn,
    );
    let (events, messages) = collect(stream).await;

    assert!(executed.lock().unwrap().is_empty());
    let (is_error, result_text) = first_tool_execution_end_outcome(&events);
    assert!(is_error);
    assert!(
        result_text.starts_with("Validation failed for tool \"echo\":"),
        "unexpected validation error text: {result_text}"
    );

    let tool_result = messages.iter().find_map(|m| match m {
        AgentMessage::ToolResult(t) => Some(t),
        _ => None,
    });
    assert_eq!(tool_result.map(|t| t.is_error), Some(true));
    assert_eq!(call_count(&state), 2);
    assert_eq!(message_roles(&messages).last().copied(), Some("assistant"));
}

#[tokio::test]
async fn after_tool_call_hook_error_degrades_to_error_result() {
    // Upstream wraps `afterToolCall` in try/catch: a failing hook replaces
    // the whole outcome with an error result carrying the error text
    // (agent-loop.ts:743-746). The tool itself executed fine; the loop
    // continues.
    let executed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![Arc::new(tool)]),
    };

    let mut config = test_config();
    config.after_tool_call = Some(Arc::new(|_hook_context, _signal| {
        Box::pin(async move { Err(AgentError::Message("after hook exploded".to_owned())) })
    }));

    let (stream_fn, state) = mock_stream_fn(vec![
        assistant_message(
            vec![tool_call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_assistant("done"),
    ]);
    let stream = agent_loop(
        vec![user_message("echo something")],
        context,
        config,
        None,
        stream_fn,
    );
    let (events, messages) = collect(stream).await;

    // The tool executed; the hook failure replaced its result.
    assert_eq!(*executed.lock().unwrap(), vec![json!("hello")]);
    let (is_error, result_text) = first_tool_execution_end_outcome(&events);
    assert!(is_error);
    assert_eq!(result_text, "after hook exploded");

    let tool_result = messages
        .iter()
        .find_map(|m| match m {
            AgentMessage::ToolResult(t) => Some(t),
            _ => None,
        })
        .expect("tool result message");
    assert!(tool_result.is_error);
    let ToolResultContent::Text(content) = &tool_result.content[0] else {
        panic!("expected text content");
    };
    assert_eq!(content.text, "after hook exploded");
    assert_eq!(call_count(&state), 2);
    assert_eq!(message_roles(&messages).last().copied(), Some("assistant"));
}
