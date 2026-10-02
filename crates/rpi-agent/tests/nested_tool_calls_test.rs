//! Nested tool calls (`ctx.executeTool()`), ported from
//! `packages/coding-agent/test/nested-tool-calls.test.ts` @ a13d35a74
//! (V16-06 FR-E): id assignment, parent-carrying events, bounded record,
//! usage summation, exclusive serialization.

use std::sync::{Arc, Mutex};

use rpi_agent::agent_loop::AgentToolCallOutcome;
use rpi_agent::nested_tool_calls::{
    NESTED_CALL_LIMITS, NestedCallRecorder, NestedCallRunner, NestedToolCallHost,
    NestedToolCallOptions, NestedToolExecutionEvent,
};
use rpi_agent::types::{
    AgentTool, AgentToolCall, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode,
};
use rpi_ai::types::{TextContent, ToolResultContent, Usage, UsageCost};
use serde_json::{Value, json};

fn text_result(text: &str) -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        details: json!({}),
        ..Default::default()
    }
}

fn usage(input: u64, cost: f64) -> Usage {
    Usage {
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
    }
}

type ToolFn = Arc<dyn Fn(&str, Value) -> AgentToolResult + Send + Sync>;

struct TestTool {
    name: String,
    execution_mode: Option<ToolExecutionMode>,
    run: ToolFn,
}

#[async_trait::async_trait]
impl AgentTool for TestTool {
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

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        Ok((self.run)(tool_call_id, params))
    }
}

/// Mock host mirroring the upstream test's `createRunner`.
struct MockHost {
    tools: Mutex<Vec<Arc<dyn AgentTool>>>,
    sequential: bool,
    events: Mutex<Vec<NestedToolExecutionEvent>>,
}

impl MockHost {
    fn new(tools: Vec<Arc<dyn AgentTool>>, sequential: bool) -> Self {
        Self {
            tools: Mutex::new(tools),
            sequential,
            events: Mutex::new(Vec::new()),
        }
    }

    fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        *self.tools.lock().unwrap() = tools;
    }
}

#[async_trait::async_trait]
impl NestedToolCallHost for MockHost {
    fn get_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.lock().unwrap().clone()
    }

    fn is_sequential(&self) -> bool {
        self.sequential
    }

    async fn run_tool_call(
        &self,
        tool_call: AgentToolCall,
        _parent_tool_call_id: String,
        _signal: Option<tokio_util::sync::CancellationToken>,
        on_update: AgentToolUpdateCallback,
    ) -> Result<AgentToolCallOutcome, rpi_agent::AgentError> {
        let tool = self
            .tools
            .lock()
            .unwrap()
            .iter()
            .find(|tool| tool.name() == tool_call.name)
            .cloned();
        let Some(tool) = tool else {
            return Ok(AgentToolCallOutcome {
                result: text_result(&format!("Tool {} not found", tool_call.name)),
                tool_call,
                is_error: true,
            });
        };
        // Forward partial updates through the sink like the real pipeline.
        let update = AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent {
                text: "partial".to_owned(),
                text_signature: None,
            })],
            details: json!({}),
            ..Default::default()
        };
        on_update(update);
        let result = tool
            .execute(
                &tool_call.id,
                Value::Object(tool_call.arguments.clone()),
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await?;
        let is_error = result.is_error.unwrap_or(false);
        Ok(AgentToolCallOutcome {
            tool_call,
            result,
            is_error,
        })
    }

    async fn emit(&self, event: NestedToolExecutionEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn event_parts(event: &NestedToolExecutionEvent) -> (&'static str, String, String) {
    match event {
        NestedToolExecutionEvent::Start {
            tool_call_id,
            parent_tool_call_id,
            ..
        } => (
            "tool_execution_start",
            tool_call_id.clone(),
            parent_tool_call_id.clone(),
        ),
        NestedToolExecutionEvent::Update {
            tool_call_id,
            parent_tool_call_id,
            ..
        } => (
            "tool_execution_update",
            tool_call_id.clone(),
            parent_tool_call_id.clone(),
        ),
        NestedToolExecutionEvent::End {
            tool_call_id,
            parent_tool_call_id,
            ..
        } => (
            "tool_execution_end",
            tool_call_id.clone(),
            parent_tool_call_id.clone(),
        ),
    }
}

fn runner_with(
    tools: Vec<Arc<dyn AgentTool>>,
    sequential: bool,
) -> (NestedCallRunner, Arc<MockHost>) {
    let host = Arc::new(MockHost::new(tools, sequential));
    (NestedCallRunner::new(host.clone()), host)
}

#[tokio::test]
async fn assigns_ids_emits_parent_events_and_records_calls() {
    let echo: Arc<dyn AgentTool> = Arc::new(TestTool {
        name: "echo".to_owned(),
        execution_mode: None,
        run: Arc::new(|_id, _params| text_result("ok")),
    });
    let (runner, host) = runner_with(vec![echo], false);

    let first = runner
        .execute(
            "call",
            "echo",
            json!({"a": 1}),
            NestedToolCallOptions::default(),
        )
        .await
        .expect("nested call");
    assert_eq!(first.tool_call.id, "call/1");
    assert!(!first.is_error);

    let missing = runner
        .execute(
            "call",
            "missing",
            json!({}),
            NestedToolCallOptions::default(),
        )
        .await
        .expect("missing tool still returns an outcome");
    assert_eq!(missing.tool_call.id, "call/2");
    assert!(missing.is_error);

    let events: Vec<(&str, String, String)> = host
        .events
        .lock()
        .unwrap()
        .iter()
        .map(event_parts)
        .collect();
    assert_eq!(
        events,
        vec![
            (
                "tool_execution_start",
                "call/1".to_owned(),
                "call".to_owned()
            ),
            (
                "tool_execution_update",
                "call/1".to_owned(),
                "call".to_owned()
            ),
            ("tool_execution_end", "call/1".to_owned(), "call".to_owned()),
            (
                "tool_execution_start",
                "call/2".to_owned(),
                "call".to_owned()
            ),
            ("tool_execution_end", "call/2".to_owned(), "call".to_owned()),
        ]
    );

    let record = runner.take_record("call").expect("record");
    let calls = record.calls.expect("calls");
    assert!(calls.complete);
    assert_eq!(calls.calls.len(), 2);
    assert_eq!(calls.calls[0].id, "call/1");
    assert_eq!(calls.calls[0].name, "echo");
    assert_eq!(calls.calls[0].arguments, Some(json!({"a": 1})));
    assert_eq!(calls.calls[0].status, "ok");
    assert!(calls.calls[0].duration_ms.is_some());
    assert_eq!(calls.calls[1].id, "call/2");
    assert_eq!(calls.calls[1].status, "error");
    assert_eq!(
        calls.calls[1].error.as_deref(),
        Some("Tool missing not found")
    );
    // The record is taken once.
    assert!(runner.take_record("call").is_none());
    assert!(runner.take_record("other").is_none());
}

#[tokio::test]
async fn records_calls_of_nested_tools_on_the_model_issued_call() {
    let host = Arc::new(MockHost::new(Vec::new(), false));
    let runner = Arc::new(NestedCallRunner::new(host.clone()));
    let inner = runner.clone();
    let middle: Arc<dyn AgentTool> = Arc::new(TestTool {
        name: "middle".to_owned(),
        execution_mode: None,
        run: Arc::new(move |tool_call_id, _params| {
            futures::executor::block_on(inner.execute(
                tool_call_id,
                "leaf",
                json!({}),
                NestedToolCallOptions::default(),
            ))
            .expect("leaf");
            AgentToolResult::default()
        }),
    });
    let leaf: Arc<dyn AgentTool> = Arc::new(TestTool {
        name: "leaf".to_owned(),
        execution_mode: None,
        run: Arc::new(|_id, _params| AgentToolResult::default()),
    });
    host.set_tools(vec![middle, leaf]);

    runner
        .execute(
            "call",
            "middle",
            json!({}),
            NestedToolCallOptions::default(),
        )
        .await
        .expect("middle");
    let record = runner.take_record("call").expect("record");
    let calls = record.calls.expect("calls").calls;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "call/1");
    assert_eq!(calls[0].name, "middle");
    assert_eq!(calls[1].id, "call/1/1");
    assert_eq!(calls[1].name, "leaf");
}

#[tokio::test]
async fn sums_the_usage_of_nested_results_at_every_depth() {
    let host = Arc::new(MockHost::new(Vec::new(), false));
    let runner = Arc::new(NestedCallRunner::new(host.clone()));
    let inner = runner.clone();
    let middle: Arc<dyn AgentTool> = Arc::new(TestTool {
        name: "middle".to_owned(),
        execution_mode: None,
        run: Arc::new(move |tool_call_id, _params| {
            futures::executor::block_on(inner.execute(
                tool_call_id,
                "leaf",
                json!({}),
                NestedToolCallOptions::default(),
            ))
            .expect("leaf");
            // Its own usage only: the leaf's usage is counted once, by the
            // recorder.
            AgentToolResult {
                content: vec![],
                details: json!({}),
                usage: Some(usage(5, 0.005)),
                ..Default::default()
            }
        }),
    });
    let leaf: Arc<dyn AgentTool> = Arc::new(TestTool {
        name: "leaf".to_owned(),
        execution_mode: None,
        run: Arc::new(|_id, _params| AgentToolResult {
            content: vec![],
            details: json!({}),
            usage: Some(usage(10, 0.01)),
            ..Default::default()
        }),
    });
    let plain: Arc<dyn AgentTool> = Arc::new(TestTool {
        name: "plain".to_owned(),
        execution_mode: None,
        run: Arc::new(|_id, _params| AgentToolResult::default()),
    });
    host.set_tools(vec![middle, leaf, plain]);

    runner
        .execute(
            "call",
            "middle",
            json!({}),
            NestedToolCallOptions::default(),
        )
        .await
        .expect("middle");
    runner
        .execute("call", "leaf", json!({}), NestedToolCallOptions::default())
        .await
        .expect("leaf");
    runner
        .execute("call", "plain", json!({}), NestedToolCallOptions::default())
        .await
        .expect("plain");
    runner
        .execute("free", "plain", json!({}), NestedToolCallOptions::default())
        .await
        .expect("plain");

    let summary = runner.take_record("call").expect("summary");
    let summed = summary.usage.expect("usage");
    assert_eq!(summed.input, 25);
    assert!((summed.cost.total - 0.025).abs() < 1e-10);
    let free = runner.take_record("free").expect("free summary");
    assert!(free.calls.is_some());
    assert!(free.usage.is_none());
}

#[tokio::test]
async fn serializes_concurrent_calls_to_sequential_tools() {
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_sequential = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let active_for_tool = active.clone();
    let max_for_tool = max_sequential.clone();
    let sequential: Arc<dyn AgentTool> = Arc::new(TestTool {
        name: "sequential".to_owned(),
        execution_mode: Some(ToolExecutionMode::Sequential),
        run: Arc::new(move |_id, _params| {
            let current = active_for_tool.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            max_for_tool.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            active_for_tool.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            AgentToolResult::default()
        }),
    });
    let parallel: Arc<dyn AgentTool> = Arc::new(TestTool {
        name: "parallel".to_owned(),
        execution_mode: None,
        run: Arc::new(|_id, _params| AgentToolResult::default()),
    });
    let (runner, _host) = runner_with(vec![sequential, parallel], false);
    let runner = Arc::new(runner);

    let mut handles = Vec::new();
    for _ in 0..3 {
        let runner = runner.clone();
        handles.push(tokio::spawn(async move {
            runner
                .execute(
                    "call",
                    "sequential",
                    json!({}),
                    NestedToolCallOptions::default(),
                )
                .await
                .expect("sequential");
        }));
    }
    for handle in handles {
        handle.await.expect("join");
    }
    assert_eq!(max_sequential.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn omits_oversized_arguments_and_drops_calls_beyond_the_limit() {
    let mut recorder = NestedCallRecorder::new();
    assert!(recorder.snapshot().is_none());

    let small = recorder.start("a", "t", &json!({"x": 1}).as_object().unwrap().clone());
    recorder.finish(small, false, "");
    let snapshot = recorder.snapshot().expect("snapshot");
    assert_eq!(snapshot.calls.len(), 1);
    assert!(snapshot.complete);
    assert_eq!(snapshot.calls[0].arguments, Some(json!({"x": 1})));

    let big_args = json!({"text": "x".repeat(NESTED_CALL_LIMITS.max_argument_bytes_per_call)});
    let big = recorder.start("b", "t", big_args.as_object().unwrap());
    recorder.finish(big, true, &"e".repeat(1000));
    let snapshot = recorder.snapshot().expect("snapshot");
    assert!(!snapshot.complete);
    assert!(snapshot.calls[1].arguments.is_none());
    assert!(
        snapshot.calls[1].arguments_bytes.unwrap() as usize
            > NESTED_CALL_LIMITS.max_argument_bytes_per_call
    );
    assert_eq!(
        snapshot.calls[1].error.as_deref().map(str::len),
        Some(NESTED_CALL_LIMITS.max_error_chars)
    );

    for i in 0..NESTED_CALL_LIMITS.max_calls {
        let index = recorder.start(&format!("c{i}"), "t", json!({}).as_object().unwrap());
        recorder.finish(index, false, "");
    }
    assert_eq!(
        recorder.snapshot().unwrap().calls.len(),
        NESTED_CALL_LIMITS.max_calls
    );
}

#[test]
fn caps_total_argument_size_and_marks_unfinished_calls_incomplete() {
    let mut recorder = NestedCallRecorder::new();
    let chunk = json!({"text": "x".repeat(7000)});
    for i in 0..6 {
        recorder.start(&format!("c{i}"), "t", chunk.as_object().unwrap());
    }
    let snapshot = recorder.snapshot().expect("snapshot");
    // 32 KiB fits four 7000-byte argument objects.
    assert_eq!(
        snapshot
            .calls
            .iter()
            .filter(|call| call.arguments.is_some())
            .count(),
        4
    );
    assert!(
        snapshot
            .calls
            .iter()
            .all(|call| call.status == "unfinished")
    );
    assert!(!snapshot.complete);
}
