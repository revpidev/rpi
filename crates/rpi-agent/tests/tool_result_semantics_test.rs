//! V16-06 FR-C/E: tool-result semantics (`structuredContent` drop rule,
//! `isError` carrying data) and the nested-call summary write on the tool
//! result message (upstream `agent-loop.ts:709-754` / `:1075-1085`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use rpi_agent::agent_loop::{
    AfterToolCallResult, AgentContext, AgentLoopConfig, AgentToolCallOutcome, RunToolCallOptions,
    agent_loop, run_tool_call,
};
use rpi_agent::messages::{AgentMessage, convert_to_llm};
use rpi_agent::nested_tool_calls::NestedCallSummary;
use rpi_agent::stream_fn::StreamFn;
use rpi_agent::types::{
    AgentEvent, AgentTool, AgentToolCall, AgentToolResult, AgentToolUpdateCallback,
};
use rpi_ai::types::{
    ApiKind, AssistantContent, AssistantMessage, AssistantRole, DoneReason, InputModality, Model,
    ModelCost, StopReason, StreamEvent, TextContent, ToolCall, ToolResultMessage, Usage, UsageCost,
    UserContent, UserMessage, UserRole,
};
use rpi_ai::types::{NestedToolCalls, ToolResultContent};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

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
        input_limits: None,
        cost: ModelCost::default(),
        prompt_cache: None,
        context_window: 8192,
        max_tokens: 2048,
        headers: None,
        compat: None,
        sampling_params: None,
    }
}

fn text_content(text: &str) -> ToolResultContent {
    ToolResultContent::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })
}

fn tool_call(name: &str) -> AgentToolCall {
    AgentToolCall {
        id: "call-1".to_owned(),
        name: name.to_owned(),
        arguments: serde_json::Map::new(),
        thought_signature: None,
        namespace: None,
    }
}

struct FixedTool {
    name: String,
    result: Mutex<Option<AgentToolResult>>,
}

#[async_trait]
impl AgentTool for FixedTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.name
    }

    fn parameters(&self) -> &Value {
        static PARAMS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMS.get_or_init(|| json!({"type": "object"}))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _signal: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        Ok(self.result.lock().unwrap().clone().unwrap_or_default())
    }
}

fn fixed_tool(name: &str, result: AgentToolResult) -> Arc<dyn AgentTool> {
    Arc::new(FixedTool {
        name: name.to_owned(),
        result: Mutex::new(Some(result)),
    })
}

fn assistant_message(content: Vec<AssistantContent>, stop: StopReason) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content,
        api: ApiKind::from("openai-responses"),
        provider: "openai".to_owned(),
        model: "mock".to_owned(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: stop,
        error_message: None,
        timestamp: 1,
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
    }
}

/// Push one scripted assistant message per call, then a final text message.
fn scripted_stream_fn(responses: Vec<AssistantMessage>) -> StreamFn {
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    Arc::new(move |_model, _context, _options| {
        let message = queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| assistant_message(vec![], StopReason::Stop));
        let stream = rpi_ai::utils::event_stream::AssistantMessageEventStream::new();
        let final_message = message.clone();
        stream.push(StreamEvent::Start {
            partial: Arc::new(message),
        });
        stream.push(StreamEvent::Done {
            reason: DoneReason::Stop,
            message: final_message,
        });
        stream.end(None);
        Box::pin(stream) as rpi_agent::BoxStream<'static, StreamEvent>
    })
}

fn config() -> AgentLoopConfig {
    AgentLoopConfig {
        model: test_model(),
        reasoning: None,
        thinking_budgets: None,
        stream_options: Default::default(),
        tool_execution: rpi_agent::types::ToolExecutionMode::Parallel,
        convert_to_llm: Arc::new(|messages| Box::pin(async move { convert_to_llm(&messages) })),
        transform_context: None,
        get_api_key: None,
        finish_turn: None,
        prepare_request: None,
        prepare_next_turn: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        before_tool_call: None,
        after_tool_call: None,
        nested_call_summary: None,
    }
}

#[tokio::test]
async fn is_error_on_the_result_marks_the_outcome_without_dropping_data() {
    let tool = fixed_tool(
        "failing",
        AgentToolResult {
            content: vec![text_content("partial data")],
            details: json!({"why": "kept"}),
            structured_content: Some(json!({"kept": true})),
            is_error: Some(true),
            ..Default::default()
        },
    );
    let context = AgentContext {
        messages: vec![],
        tools: Some(vec![tool]),
    };
    let outcome = run_tool_call(
        tool_call("failing"),
        RunToolCallOptions {
            tools: context.tools.clone().unwrap_or_default(),
            assistant_message: assistant_message(vec![], StopReason::ToolUse),
            context,
            before_tool_call: None,
            after_tool_call: None,
            signal: None,
            on_update: None,
        },
    )
    .await;
    assert!(outcome.is_error);
    assert_eq!(outcome.result.content, vec![text_content("partial data")]);
    assert_eq!(
        outcome.result.structured_content,
        Some(json!({"kept": true}))
    );
}

#[tokio::test]
async fn after_tool_call_drops_structured_content_only_when_content_is_replaced() {
    let tool = fixed_tool(
        "read",
        AgentToolResult {
            content: vec![text_content("original")],
            details: json!({}),
            structured_content: Some(json!({"original": true})),
            ..Default::default()
        },
    );
    let context = AgentContext {
        messages: vec![],
        tools: Some(vec![tool]),
    };
    // Content-only patch → structured content dropped.
    let dropped = run_tool_call(
        tool_call("read"),
        RunToolCallOptions {
            tools: context.tools.clone().unwrap_or_default(),
            assistant_message: assistant_message(vec![], StopReason::ToolUse),
            context: context.clone(),
            before_tool_call: None,
            after_tool_call: Some(Arc::new(|_context, _signal| {
                Box::pin(async move {
                    Ok(Some(AfterToolCallResult {
                        content: Some(vec![text_content("redacted")]),
                        ..Default::default()
                    }))
                })
            })),
            signal: None,
            on_update: None,
        },
    )
    .await;
    assert!(dropped.result.structured_content.is_none());
    assert_eq!(dropped.result.content, vec![text_content("redacted")]);

    // Details-only patch → structured content preserved.
    let preserved = run_tool_call(
        tool_call("read"),
        RunToolCallOptions {
            tools: context.tools.clone().unwrap_or_default(),
            assistant_message: assistant_message(vec![], StopReason::ToolUse),
            context,
            before_tool_call: None,
            after_tool_call: Some(Arc::new(|_context, _signal| {
                Box::pin(async move {
                    Ok(Some(AfterToolCallResult {
                        details: Some(json!({"patched": true})),
                        ..Default::default()
                    }))
                })
            })),
            signal: None,
            on_update: None,
        },
    )
    .await;
    assert_eq!(
        preserved.result.structured_content,
        Some(json!({"original": true}))
    );
}

#[tokio::test]
async fn loop_writes_nested_calls_and_combined_usage_on_the_message() {
    let usage = |input: u64, cost: f64| Usage {
        input,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write1h: None,
        reasoning: None,
        total_tokens: input,
        cost: UsageCost {
            input: cost,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: cost,
        },
    };
    let tool = fixed_tool(
        "orchestrator",
        AgentToolResult {
            content: vec![text_content("done")],
            details: json!({}),
            usage: Some(usage(5, 0.005)),
            ..Default::default()
        },
    );
    let stream_fn = scripted_stream_fn(vec![
        assistant_message(
            vec![AssistantContent::ToolCall(ToolCall {
                id: "call-1".to_owned(),
                name: "orchestrator".to_owned(),
                arguments: serde_json::Map::new(),
                thought_signature: None,
                namespace: None,
            })],
            StopReason::ToolUse,
        ),
        assistant_message(
            vec![AssistantContent::Text(TextContent {
                text: "final".to_owned(),
                text_signature: None,
            })],
            StopReason::Stop,
        ),
    ]);
    let mut config = config();
    config.nested_call_summary = Some(Arc::new(move |tool_call_id: &str| {
        if tool_call_id != "call-1" {
            return None;
        }
        Some(NestedCallSummary {
            calls: Some(NestedToolCalls {
                calls: vec![rpi_ai::types::NestedToolCallRecord {
                    id: "call-1/1".to_owned(),
                    name: "helper".to_owned(),
                    arguments: Some(json!({})),
                    arguments_bytes: None,
                    status: "ok".to_owned(),
                    duration_ms: Some(1),
                    error: None,
                }],
                complete: true,
            }),
            usage: Some(usage(10, 0.01)),
        })
    }));
    let context = AgentContext {
        messages: vec![],
        tools: Some(vec![tool]),
    };
    let mut stream = agent_loop(
        vec![AgentMessage::User(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("go".to_owned()),
            timestamp: 0,
        })],
        context,
        config,
        None,
        stream_fn,
    );
    let mut tool_result_message: Option<ToolResultMessage> = None;
    while let Some(event) = stream.next().await {
        if let AgentEvent::MessageEnd {
            message: AgentMessage::ToolResult(message),
        } = event
        {
            tool_result_message = Some(message);
        }
    }
    let result = tool_result_message.expect("tool result message");
    let nested = result.nested_calls.expect("nested calls");
    assert_eq!(nested.calls.len(), 1);
    assert_eq!(nested.calls[0].id, "call-1/1");
    assert!(nested.complete);
    let usage = result.usage.expect("combined usage");
    assert_eq!(usage.input, 15);
    assert!((usage.cost.total - 0.015).abs() < 1e-10);
}

#[tokio::test]
async fn outcome_is_printable_for_nested_hosts() {
    // Compile-time usefulness check for the public outcome shape.
    let outcome = AgentToolCallOutcome {
        tool_call: tool_call("x"),
        result: AgentToolResult::default(),
        is_error: false,
    };
    assert_eq!(outcome.tool_call.name, "x");
}
