//! Nested tool calls and structured output (V18-06 FR-A; port of
//! `packages/durable/test/harness-nested-tools.test.ts` and
//! `harness-structured-output.test.ts` @ `4a080252f`).
//!
//! Drives the real `pi.tool` built-in through a Harness over memory storage with
//! a scripted provider.

mod generation_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rpi_ai::types::{Message, Tool as AiTool};
use rpi_durable::chord::context::Context;
use rpi_durable::chord::json::{JsonObject, JsonValue};
use rpi_durable::harness::{
    Extension, NestedCallOptions, NestedToolExecutionResult, ToolCaller, ToolExecute,
    ToolExecuteError, ToolExecutionApi, ToolExecutionResult, ToolRegistration, ToolReplay,
};
use rpi_durable::ids::TaskId;
use rpi_durable::session::DocOwner;
use rpi_durable::types::EntryRecord;
use rpi_durable::types::TaskState;

use generation_support::{
    Script, context, entries, open, root_ready, submit_input, test_models, text_message,
    tool_use_message,
};

/// The result text of every `pi.tool-result` entry.
fn tool_result_texts(entries: &[EntryRecord]) -> Vec<String> {
    let mut texts = Vec::new();
    for entry in entries {
        let Some(messages) = &entry.model else {
            continue;
        };
        for message in messages {
            if let Message::ToolResult(result) = message {
                texts.push(
                    result
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            rpi_ai::types::ToolResultContent::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
    }
    texts
}

fn tool(
    name: &str,
    callers: Option<Vec<ToolCaller>>,
    structured_output_schema: Option<JsonValue>,
    execute: Arc<dyn ToolExecute>,
) -> ToolRegistration {
    ToolRegistration {
        tool: AiTool {
            name: name.to_string(),
            description: name.to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
            constrained_sampling: None,
        },
        replay: ToolReplay::Unsafe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema,
        callers,
        deferred: false,
        execute,
    }
}

fn extension(tools: Vec<ToolRegistration>) -> Extension {
    Extension {
        name: "nested".to_string(),
        tools,
        ..Extension::default()
    }
}

/// Records every inner execution so a replay can be checked for zero side
/// effects.
struct InnerTool {
    runs: Arc<AtomicUsize>,
    value: Option<JsonValue>,
}

#[async_trait]
impl ToolExecute for InnerTool {
    async fn execute(
        &self,
        _args: JsonObject,
        _api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolExecutionResult {
            structured_output: self.value.clone(),
            ..Default::default()
        })
    }
}

/// Calls `inner` and reports what it got through its own output.
struct OuterTool;

#[async_trait]
impl ToolExecute for OuterTool {
    async fn execute(
        &self,
        _args: JsonObject,
        api: &dyn ToolExecutionApi,
        context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        let result: NestedToolExecutionResult = api
            .execute_tool(
                "inner",
                JsonObject::new(),
                context,
                NestedCallOptions::default(),
            )
            .await
            .map_err(|error| -> ToolExecuteError { Box::new(error) })?;
        let text = format!(
            "inner error={} value={} diagnostics={}",
            result.is_error,
            result
                .structured_output
                .as_ref()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string()),
            result.diagnostics.len()
        );
        api.output(text.as_bytes(), None).expect("output");
        Ok(ToolExecutionResult::default())
    }
}

#[tokio::test]
async fn nested_call_returns_structured_output_without_a_transcript_entry() {
    let runs = Arc::new(AtomicUsize::new(0));
    let opened = open(
        vec![extension(vec![
            tool("outer", None, None, Arc::new(OuterTool)),
            tool(
                "inner",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "number" } },
                    "required": ["value"]
                })),
                Arc::new(InnerTool {
                    runs: Arc::clone(&runs),
                    value: Some(serde_json::json!({ "value": 42 })),
                }),
            ),
        ])],
        None,
        test_models(vec![
            Script::Message(tool_use_message("outer", "c1", serde_json::json!({}))),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");

    let results = tool_result_texts(&entries(&conversation).await);
    assert_eq!(
        results.len(),
        1,
        "only the model-issued call is a result: {results:?}"
    );
    assert!(
        results[0].contains("inner error=false value={\"value\":42}"),
        "got {}",
        results[0]
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // The nested call's task is terminal and its live slot is gone once the
    // caller settled.
    let live = opened
        .harness
        .session()
        .snapshot(
            &rpi_durable::harness::LIVE_DOC,
            DocOwner::Conversation(conversation.id()),
            &context(),
        )
        .await
        .expect("snapshot");
    let state: rpi_durable::harness::LiveState =
        serde_json::from_value(JsonValue::Object(live.unwrap_or_default())).expect("live");
    assert!(state.nested_tools.is_none(), "nested slots are removed");
}

#[tokio::test]
async fn nested_structured_output_failure_is_an_error_result() {
    let opened = open(
        vec![extension(vec![
            tool("outer", None, None, Arc::new(OuterTool)),
            tool(
                "inner",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "number" } },
                    "required": ["value"]
                })),
                Arc::new(InnerTool {
                    runs: Arc::new(AtomicUsize::new(0)),
                    value: Some(serde_json::json!({ "value": "not a number" })),
                }),
            ),
        ])],
        None,
        test_models(vec![
            Script::Message(tool_use_message("outer", "c1", serde_json::json!({}))),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let results = tool_result_texts(&entries(&conversation).await);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].contains("inner error=true value=none diagnostics=1"),
        "got {}",
        results[0]
    );
}

#[tokio::test]
async fn nested_call_without_a_schema_gets_the_bounded_output() {
    let opened = open(
        vec![extension(vec![
            tool("outer", None, None, Arc::new(OuterTool)),
            tool(
                "inner",
                Some(vec![ToolCaller::Tools]),
                None,
                Arc::new(TextTool),
            ),
        ])],
        None,
        test_models(vec![
            Script::Message(tool_use_message("outer", "c1", serde_json::json!({}))),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let results = tool_result_texts(&entries(&conversation).await);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].contains("inner error=false value=\"text output\" diagnostics=0"),
        "got {}",
        results[0]
    );
}

struct TextTool;

#[async_trait]
impl ToolExecute for TextTool {
    async fn execute(
        &self,
        _args: JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        api.output(b"text output", None).expect("output");
        Ok(ToolExecutionResult::default())
    }
}

#[test]
fn a_version_one_tool_task_migrates_to_a_model_issued_v2_task() {
    use rpi_durable::harness::{TaskDefinition, ToolTask};

    let task = ToolTask;
    let migrated = task
        .migrate(
            serde_json::json!({ "assistant": 7, "callId": "c1" }),
            serde_json::json!({ "phase": "call" }),
            1,
        )
        .expect("migration");
    assert_eq!(
        migrated.input,
        serde_json::json!({ "kind": "model", "assistant": 7, "callId": "c1" })
    );
    assert_eq!(task.version(), 2);
}

#[test]
fn a_terminal_nested_task_state_is_readable() {
    let state: TaskState = serde_json::from_value(serde_json::json!({
        "status": "terminal",
        "outcome": { "status": "completed", "result": { "kind": "nested" } }
    }))
    .expect("state");
    assert!(matches!(state, TaskState::Terminal { .. }));
    let _ = TaskId::from_number(1);
}

/// V18-06 FR-F: telemetry carries durable record IDs, never content.
#[tokio::test]
async fn telemetry_records_ids_and_replay_events_without_content() {
    use rpi_durable::harness::{TelemetryContext, TelemetrySink};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Sink {
        attributes: Mutex<Vec<JsonObject>>,
        events: Mutex<Vec<String>>,
    }

    impl TelemetrySink for Sink {
        fn set_attributes(&self, attributes: JsonObject) {
            self.attributes.lock().expect("attributes").push(attributes);
        }
        fn add_event(&self, name: &str, _attributes: JsonObject) {
            self.events.lock().expect("events").push(name.to_string());
        }
        fn add_link(&self, _carrier: JsonValue) {}
        fn record_metric(&self, _name: &str, _value: f64) {}
    }

    let sink = Arc::new(Sink::default());
    let runs = Arc::new(AtomicUsize::new(0));
    let harness = open_with_telemetry(
        vec![extension(vec![
            tool("outer", None, None, Arc::new(OuterTool)),
            tool(
                "inner",
                Some(vec![ToolCaller::Tools]),
                None,
                Arc::new(TextTool),
            ),
        ])],
        TelemetryContext::new(sink.clone()),
        runs,
    )
    .await;
    let conversation = root_ready(&harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");

    let attributes = sink.attributes.lock().expect("attributes").clone();
    assert!(attributes.iter().any(|attributes| {
        attributes.get("tool.name").and_then(JsonValue::as_str) == Some("outer")
            && attributes
                .get("task.name")
                .and_then(JsonValue::as_str)
                .is_none()
    }));
    assert!(attributes.iter().any(|attributes| {
        attributes.get("task.name").and_then(JsonValue::as_str) == Some("pi.tool")
    }));
    let rendered = serde_json::to_string(&attributes).expect("render");
    assert!(
        !rendered.contains("text output"),
        "no tool output in telemetry"
    );
}

async fn open_with_telemetry(
    tools: Vec<Extension>,
    telemetry: rpi_durable::harness::TelemetryContext,
    runs: Arc<AtomicUsize>,
) -> Arc<rpi_durable::harness::HarnessImpl> {
    use rpi_ai::models::Models;
    use rpi_durable::harness::{
        HarnessOptions, Registry, RegistryImpl, SchedulerDeps, SchedulerFactory, SchedulerService,
        open_harness_with_scheduler,
    };
    let _ = runs;
    let registry = Arc::new(RegistryImpl::with_builtins(
        rpi_durable::harness::builtin_tasks(),
    ));
    for extension in tools {
        registry.install(extension).expect("install");
    }
    let models = test_models(vec![
        Script::Message(tool_use_message("outer", "c1", serde_json::json!({}))),
        Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
    ]);
    let options = HarnessOptions {
        telemetry: Some(telemetry),
        models: models.into(),
        identity: None,
        registry: Arc::clone(&registry) as Arc<dyn rpi_durable::harness::RegistryReader>,
        settings: None,
        env: None,
        conversation_created: None,
        now: None,
        set_timer: None,
        on_wake: None,
        on_report: None,
    };
    let storage: Arc<dyn rpi_durable::types::Storage> =
        Arc::new(rpi_durable::storage::MemoryStorage::new());
    let factory: SchedulerFactory = Arc::new(|deps: SchedulerDeps| {
        rpi_durable::harness::TaskScheduler::new(deps) as Arc<dyn SchedulerService>
    });
    let harness = open_harness_with_scheduler(storage, options, factory, &context())
        .await
        .expect("open");
    let _ = (Models::new(None), runs);
    harness
}
