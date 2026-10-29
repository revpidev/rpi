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
        on_report_info: None,
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

/// V18-06 FR-B: two calls with the same key run the nested tool once; the
/// second is answered from the journal.
#[tokio::test]
async fn codemode_journal_replays_a_delivered_nested_result() {
    use rpi_durable::codemode::{
        CODEMODE_TOOL_NAME, CodemodeHost, CodemodeOutcome, CodemodeSandbox, CodemodeStart,
        CodemodeToolOptions, create_codemode_tool,
    };

    struct TwiceSandbox;
    #[async_trait]
    impl CodemodeSandbox for TwiceSandbox {
        fn fingerprint(&self) -> String {
            "fake-1".to_string()
        }
        async fn run(
            &self,
            _start: CodemodeStart,
            host: &dyn CodemodeHost,
            _context: &Context,
        ) -> Result<CodemodeOutcome, String> {
            let first = host.call_tool("inner", JsonObject::new(), "c1").await?;
            let second = host.call_tool("inner", JsonObject::new(), "c1").await?;
            Ok(CodemodeOutcome::output(format!(
                "value={} again={}",
                first
                    .structured_output
                    .as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                second
                    .structured_output
                    .as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            )))
        }
    }

    let runs = Arc::new(AtomicUsize::new(0));
    let code = create_codemode_tool(CodemodeToolOptions {
        sandbox: Arc::new(TwiceSandbox),
    });
    let opened = open(
        vec![extension(vec![
            code,
            tool(
                "inner",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({
                    "type": "object",
                    "properties": { "answer": { "type": "number" } },
                    "required": ["answer"]
                })),
                Arc::new(InnerTool {
                    runs: Arc::clone(&runs),
                    value: Some(serde_json::json!({ "answer": 7 })),
                }),
            ),
        ])],
        None,
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "return 1" }),
            )),
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
        results[0].contains("value={\"answer\":7}"),
        "got {}",
        results[0]
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the keyed call runs once");
}

#[test]
fn codemode_journal_keeps_the_first_result_per_key() {
    use rpi_durable::codemode::{CodemodeJournal, CodemodeJournalEntry};
    use rpi_durable::harness::NestedToolExecutionResult;

    let result = |value: i64| NestedToolExecutionResult {
        task_id: TaskId::from_number(1),
        structured_output: Some(serde_json::json!({ "value": value })),
        is_error: false,
        details: None,
        diagnostics: Vec::new(),
        usage: None,
        duration_ms: None,
    };
    let mut journal = CodemodeJournal::default();
    journal.record("1".to_string(), "inner".to_string(), result(1));
    journal.record("1".to_string(), "inner".to_string(), result(2));
    assert_eq!(
        journal
            .get("1")
            .and_then(|entry| entry.structured_output.clone()),
        Some(serde_json::json!({ "value": 1 }))
    );
    let encoded = serde_json::to_value(CodemodeJournalEntry {
        key: "1".to_string(),
        name: "inner".to_string(),
        result: result(1),
    })
    .expect("encode");
    assert_eq!(encoded["key"], serde_json::json!("1"));
}

// `tool_result_texts` above skips the harness's structured results; this test
// module reuses it because the codemode result is a text item.

/// V18-06 FR-B: a crash before the code mode call settles reruns the script
/// from its stored start data; the journal answers its nested call, so the
/// tool executes once.
#[tokio::test]
async fn codemode_replay_after_restart_does_not_repeat_a_nested_call() {
    use rpi_durable::codemode::{
        CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, CODEMODE_TOOL_NAME, CodemodeHost, CodemodeJournal,
        CodemodeOutcome, CodemodeSandbox, CodemodeStart, CodemodeToolOptions, create_codemode_tool,
    };
    use rpi_durable::harness::{Registry, TaskScheduler, open_harness_with_scheduler};
    use rpi_durable::session::{EntryDraft, TaskCreate, TaskOwnership, commit_fn};
    use rpi_durable::types::TaskState;

    struct HeldSandbox {
        called: Arc<tokio::sync::Notify>,
        hold: Arc<tokio::sync::Notify>,
        attempts: AtomicUsize,
    }
    #[async_trait]
    impl CodemodeSandbox for HeldSandbox {
        fn fingerprint(&self) -> String {
            "fake-1".to_string()
        }
        async fn run(
            &self,
            _start: CodemodeStart,
            host: &dyn CodemodeHost,
            _context: &Context,
        ) -> Result<CodemodeOutcome, String> {
            let result = host.call_tool("inner", JsonObject::new(), "c1").await?;
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                self.called.notify_one();
                // The first process "crashes" here: it never returns.
                self.hold.notified().await;
            }
            Ok(CodemodeOutcome::output(format!(
                "value={}",
                result
                    .structured_output
                    .as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            )))
        }
    }

    let runs = Arc::new(AtomicUsize::new(0));
    let called = Arc::new(tokio::sync::Notify::new());
    let hold = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(rpi_durable::harness::RegistryImpl::with_builtins(
        rpi_durable::harness::builtin_tasks(),
    ));
    registry
        .install(extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::new(HeldSandbox {
                    called: Arc::clone(&called),
                    hold: Arc::clone(&hold),
                    attempts: AtomicUsize::new(0),
                }),
            }),
            tool(
                "inner",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({
                    "type": "object",
                    "properties": { "answer": { "type": "number" } },
                    "required": ["answer"]
                })),
                Arc::new(InnerTool {
                    runs: Arc::clone(&runs),
                    value: Some(serde_json::json!({ "answer": 7 })),
                }),
            ),
        ]))
        .expect("install");
    let storage: Arc<dyn rpi_durable::types::Storage> =
        Arc::new(rpi_durable::storage::MemoryStorage::new());
    let options = || rpi_durable::harness::HarnessOptions {
        telemetry: None,
        models: test_models(Vec::new()).into(),
        identity: None,
        registry: Arc::clone(&registry) as Arc<dyn rpi_durable::harness::RegistryReader>,
        settings: None,
        env: None,
        conversation_created: None,
        now: None,
        set_timer: None,
        on_wake: None,
        on_report: None,
        on_report_info: None,
    };
    let factory: rpi_durable::harness::SchedulerFactory = Arc::new(|deps| {
        TaskScheduler::new(deps) as Arc<dyn rpi_durable::harness::SchedulerService>
    });
    let harness = open_harness_with_scheduler(
        Arc::clone(&storage),
        options(),
        Arc::clone(&factory),
        &context(),
    )
    .await
    .expect("open");
    let conversation = harness
        .root(&context(), Default::default())
        .await
        .expect("root")
        .id();
    let call = tool_use_message(
        CODEMODE_TOOL_NAME,
        "c1",
        serde_json::json!({ "code": "return 1" }),
    );
    let task_id = harness
        .session()
        .commit(
            commit_fn(move |tx| {
                let call = call.clone();
                Box::pin(async move {
                    let mut draft = EntryDraft::new("pi.assistant");
                    draft.model = Some(vec![Message::Assistant(call)]);
                    let entry = tx.append_entry(conversation, draft).await?;
                    tx.create_task(TaskCreate {
                        kind: "pi.tool".to_string(),
                        version: 2,
                        input: serde_json::json!({
                            "kind": "model",
                            "assistant": entry.id,
                            "callId": "c1",
                        }),
                        checkpoint: serde_json::json!({ "phase": "call" }),
                        ownership: TaskOwnership::Conversation(conversation),
                        conversation_id: Some(conversation),
                        background: false,
                        abandon_on_restart: false,
                    })
                    .await
                })
            }),
            &context(),
        )
        .await
        .expect("create tool task");
    harness.resume().expect("resume");
    called.notified().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // The first process held the call after its nested result was journaled;
    // reopening recovers the running task and reruns the script.
    let journal = harness
        .session()
        .snapshot(&CODEMODE_JOURNAL_DOC, DocOwner::Task(task_id), &context())
        .await
        .expect("journal")
        .expect("present");
    let journal: CodemodeJournal =
        serde_json::from_value(JsonValue::Object(journal)).expect("journal json");
    assert!(journal.get("c1").is_some());
    assert!(
        harness
            .session()
            .snapshot(&CODEMODE_RUN_DOC, DocOwner::Task(task_id), &context())
            .await
            .expect("run")
            .is_some()
    );

    let reopened = open_harness_with_scheduler(
        Arc::clone(&storage),
        options(),
        Arc::clone(&factory),
        &context(),
    )
    .await
    .expect("reopen");
    reopened.resume().expect("resume");
    let settled = reopened
        .wait_for_task(task_id, &context())
        .await
        .expect("replayed run");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the journal answered the nested call"
    );
    assert!(matches!(settled.record.state, TaskState::Terminal { .. }));
    drop(hold);
}

/// V18-06 FR-K: one harness runs a migrated v1 tool task with a nested call
/// and then a sleeping task that wakes at its checkpoint.
#[tokio::test]
async fn end_to_end_v1_migration_nested_and_sleep() {
    use rpi_durable::harness::{
        Harness, NextTaskState, Registry, TaskDefinition, TaskRuntime, TaskScheduler,
        open_harness_with_scheduler,
    };
    use rpi_durable::session::{EntryDraft, TaskCreate, TaskOwnership, commit_fn};
    use rpi_durable::types::TaskState;

    struct SleepThenDone(Arc<AtomicUsize>);
    #[async_trait]
    impl TaskDefinition for SleepThenDone {
        fn name(&self) -> &str {
            "test.sleep"
        }
        fn version(&self) -> u64 {
            1
        }
        fn initial(&self) -> JsonValue {
            serde_json::json!({ "phase": "sleep" })
        }
        async fn run_phase(
            &self,
            phase: &str,
            _task: rpi_durable::harness::RunningTask,
            runtime: &dyn TaskRuntime,
            context: &Context,
        ) -> rpi_durable::session::SessionResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if phase == "sleep" {
                let until = runtime.now()? + 10.0;
                runtime
                    .sleep_until(until, serde_json::json!({ "phase": "done" }), context)
                    .await
            } else {
                runtime
                    .commit(
                        Arc::new(|_tx, _current| {
                            Box::pin(async move {
                                Ok(Some(NextTaskState::Terminal {
                                    outcome: rpi_durable::types::TaskOutcome::Completed {
                                        result: serde_json::json!({ "woke": true }),
                                    },
                                }))
                            })
                        }),
                        context,
                    )
                    .await
            }
        }
    }

    struct Outer;
    #[async_trait]
    impl ToolExecute for Outer {
        async fn execute(
            &self,
            _args: JsonObject,
            api: &dyn ToolExecutionApi,
            context: &Context,
        ) -> Result<ToolExecutionResult, ToolExecuteError> {
            let inner = api
                .execute_tool(
                    "inner",
                    JsonObject::new(),
                    context,
                    NestedCallOptions::default(),
                )
                .await
                .map_err(|error| -> ToolExecuteError { Box::new(error) })?;
            api.output(
                format!(
                    "inner={}",
                    inner
                        .structured_output
                        .as_ref()
                        .map(|value| value.to_string())
                        .unwrap_or_default()
                )
                .as_bytes(),
                None,
            )
            .expect("output");
            Ok(ToolExecutionResult::default())
        }
    }

    let runs = Arc::new(AtomicUsize::new(0));
    let phases = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(rpi_durable::harness::RegistryImpl::with_builtins(
        rpi_durable::harness::builtin_tasks(),
    ));
    registry
        .install(extension(vec![
            tool("outer", None, None, Arc::new(Outer)),
            tool(
                "inner",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({
                    "type": "object",
                    "properties": { "answer": { "type": "number" } },
                    "required": ["answer"]
                })),
                Arc::new(InnerTool {
                    runs: Arc::clone(&runs),
                    value: Some(serde_json::json!({ "answer": 7 })),
                }),
            ),
        ]))
        .expect("install tools");
    registry
        .install(Extension {
            name: "sleep".to_string(),
            tasks: vec![Arc::new(SleepThenDone(Arc::clone(&phases)))],
            ..Extension::default()
        })
        .expect("install sleep task");
    let storage: Arc<dyn rpi_durable::types::Storage> =
        Arc::new(rpi_durable::storage::MemoryStorage::new());
    let options = rpi_durable::harness::HarnessOptions {
        telemetry: None,
        models: test_models(Vec::new()).into(),
        identity: None,
        registry: Arc::clone(&registry) as Arc<dyn rpi_durable::harness::RegistryReader>,
        settings: None,
        env: None,
        conversation_created: None,
        now: None,
        set_timer: None,
        on_wake: None,
        on_report: None,
        on_report_info: None,
    };
    let factory: rpi_durable::harness::SchedulerFactory = Arc::new(|deps| {
        TaskScheduler::new(deps) as Arc<dyn rpi_durable::harness::SchedulerService>
    });
    let harness = open_harness_with_scheduler(Arc::clone(&storage), options, factory, &context())
        .await
        .expect("open");
    let conversation = harness
        .root(&context(), Default::default())
        .await
        .expect("root")
        .id();
    let call = tool_use_message("outer", "c1", serde_json::json!({}));
    let (tool_task, sleep_task) = harness
        .session()
        .commit(
            commit_fn(move |tx| {
                let call = call.clone();
                Box::pin(async move {
                    let mut draft = EntryDraft::new("pi.assistant");
                    draft.model = Some(vec![Message::Assistant(call)]);
                    let entry = tx.append_entry(conversation, draft).await?;
                    let tool_task = tx
                        .create_task(TaskCreate {
                            kind: "pi.tool".to_string(),
                            version: 1,
                            input: serde_json::json!({
                                "assistant": entry.id,
                                "callId": "c1",
                            }),
                            checkpoint: serde_json::json!({ "phase": "call" }),
                            ownership: TaskOwnership::Conversation(conversation),
                            conversation_id: Some(conversation),
                            background: false,
                            abandon_on_restart: false,
                        })
                        .await?;
                    let sleep_task = tx
                        .create_task(TaskCreate {
                            kind: "test.sleep".to_string(),
                            version: 1,
                            input: JsonValue::Null,
                            checkpoint: serde_json::json!({ "phase": "sleep" }),
                            ownership: TaskOwnership::Conversation(conversation),
                            conversation_id: Some(conversation),
                            background: false,
                            abandon_on_restart: false,
                        })
                        .await?;
                    Ok((tool_task, sleep_task))
                })
            }),
            &context(),
        )
        .await
        .expect("create tasks");
    harness.resume().expect("resume");
    let tool = harness
        .wait_for_task(tool_task, &context())
        .await
        .expect("tool task");
    assert_eq!(tool.record.version, 2, "v1 task migrated");
    assert!(matches!(tool.record.state, TaskState::Terminal { .. }));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let sleep = harness
        .wait_for_task(sleep_task, &context())
        .await
        .expect("sleep task");
    assert!(matches!(sleep.record.state, TaskState::Terminal { .. }));
    assert_eq!(phases.load(Ordering::SeqCst), 2);
    let _ = Harness::wake(&*harness);
}
