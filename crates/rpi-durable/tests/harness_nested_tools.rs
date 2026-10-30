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
        fn record_metric(&self, _name: &str, _value: f64, _attributes: JsonObject) {}
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
        ..Default::default()
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
    let args = rpi_durable::chord::json::JsonObject::new();
    journal
        .record("1", "inner", &args, 0.0, result(1))
        .expect("record");
    journal
        .record("1", "inner", &args, 0.0, result(2))
        .expect("same call");
    assert_eq!(
        journal
            .get("1")
            .and_then(|entry| entry.structured_output.clone()),
        Some(serde_json::json!({ "value": 1 }))
    );
    let encoded = serde_json::to_value(CodemodeJournalEntry {
        key: "1".to_string(),
        name: "inner".to_string(),
        result: Some(result(1)),
        ..Default::default()
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
async fn v1_migration_nested_replay_then_sleep_is_one_chain() {
    use rpi_durable::codemode::{
        CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, CODEMODE_TOOL_NAME, CodemodeHost, CodemodeJournal,
        CodemodeOutcome, CodemodeSandbox, CodemodeStart, CodemodeToolOptions, create_codemode_tool,
    };
    use rpi_durable::harness::{Registry, TaskScheduler, open_harness_with_scheduler};
    use rpi_durable::session::{EntryDraft, TaskCreate, TaskOwnership, commit_fn};
    use rpi_durable::types::TaskState;

    struct SleepThenDone(Arc<AtomicUsize>);
    #[async_trait]
    impl rpi_durable::harness::TaskDefinition for SleepThenDone {
        fn name(&self) -> &str {
            "test.chain.sleep"
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
            runtime: &dyn rpi_durable::harness::TaskRuntime,
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
                                Ok(Some(rpi_durable::harness::NextTaskState::Terminal {
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
                "value={} reply={}",
                result
                    .structured_output
                    .as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                host.start_reply()
            )))
        }
    }

    let runs = Arc::new(AtomicUsize::new(0));
    let phases = Arc::new(AtomicUsize::new(0));
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
                ..Default::default()
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
    registry
        .install(rpi_durable::harness::Extension {
            name: "chain-sleep".to_string(),
            tasks: vec![Arc::new(SleepThenDone(Arc::clone(&phases)))],
            ..rpi_durable::harness::Extension::default()
        })
        .expect("install sleep task");
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
                        trace: Default::default(),
                        links: Vec::new(),
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
    let run = harness
        .session()
        .snapshot(&CODEMODE_RUN_DOC, DocOwner::Task(task_id), &context())
        .await
        .expect("run")
        .expect("run document");
    let run: rpi_durable::codemode::CodemodeRunState =
        serde_json::from_value(JsonValue::Object(run)).expect("run state");
    let start = run.start.expect("start data");
    assert_eq!(
        start.version,
        rpi_durable::codemode::CODEMODE_MODULE_VERSION
    );
    assert!(!start.tools.is_empty());
    assert!(start.max_output_tokens > 0);
    assert_eq!(run.start_reply, Some(JsonValue::Null));

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
    assert_eq!(settled.record.version, 2, "the v1 task migrated to v2");
    assert!(matches!(
        settled.record.input,
        JsonValue::Object(ref input) if input.get("kind").and_then(JsonValue::as_str) == Some("model")
    ));
    assert!(matches!(settled.record.state, TaskState::Terminal { .. }));
    let texts = tool_result_texts(&entries(&root_ready(&reopened).await).await);
    assert!(
        texts.iter().any(|text| text.contains("reply=null")),
        "the replay reused the stored start reply: {texts:?}"
    );

    // The same chain continues into a sleeping phase that wakes at its
    // checkpoint after the replay has settled.
    let sleep_task = reopened
        .session()
        .commit(
            commit_fn(move |tx| {
                Box::pin(async move {
                    tx.create_task(TaskCreate {
                        kind: "test.chain.sleep".to_string(),
                        version: 1,
                        input: JsonValue::Null,
                        checkpoint: serde_json::json!({ "phase": "sleep" }),
                        ownership: TaskOwnership::Conversation(conversation),
                        conversation_id: Some(conversation),
                        background: false,
                        abandon_on_restart: false,
                        trace: Default::default(),
                        links: Vec::new(),
                    })
                    .await
                })
            }),
            &context(),
        )
        .await
        .expect("create sleep task");
    let slept = reopened
        .wait_for_task(sleep_task, &context())
        .await
        .expect("sleep task");
    assert!(matches!(slept.record.state, TaskState::Terminal { .. }));
    assert_eq!(phases.load(Ordering::SeqCst), 2);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "no second side effect");
    drop(hold);
}

/// V18-06 FR-K: one harness runs a migrated v1 tool task with a nested call
/// and then a sleeping task that wakes at its checkpoint.
#[tokio::test]
async fn end_to_end_v1_migration_nested_and_sleep() {
    use rpi_durable::harness::{
        NextTaskState, Registry, TaskDefinition, TaskRuntime, TaskScheduler,
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
                            trace: Default::default(),
                            links: Vec::new(),
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
                            trace: Default::default(),
                            links: Vec::new(),
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
}

/// FR-B R3/R4: concurrent calls are delivered in journal order after a reopen,
/// without rerunning either nested tool.
#[tokio::test]
async fn concurrent_codemode_calls_replay_in_delivery_order() {
    use futures::{StreamExt, stream::FuturesUnordered};
    use generation_support::open_with_storage;
    use rpi_durable::codemode::{
        CODEMODE_JOURNAL_DOC, CODEMODE_TOOL_NAME, CodemodeHost, CodemodeJournal, CodemodeOutcome,
        CodemodeSandbox, CodemodeStart, CodemodeToolOptions, create_codemode_tool,
    };
    use rpi_durable::storage::MemoryStorage;
    use rpi_durable::types::Storage;
    use std::pin::Pin;

    struct OrderedSandbox {
        attempts: AtomicUsize,
        orders: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        fast_delivered: Arc<tokio::sync::Notify>,
        journaled: Arc<tokio::sync::Notify>,
        hold: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl CodemodeSandbox for OrderedSandbox {
        fn fingerprint(&self) -> String {
            "ordered-replay-1".to_string()
        }

        async fn run(
            &self,
            _start: CodemodeStart,
            host: &dyn CodemodeHost,
            _context: &Context,
        ) -> Result<CodemodeOutcome, String> {
            type CallFuture<'a> =
                Pin<Box<dyn Future<Output = (String, NestedToolExecutionResult)> + Send + 'a>>;
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 2 {
                host.call_tool("slow", JsonObject::new(), "c1").await?;
                return Err("sequential out-of-order replay was accepted".to_string());
            }
            let mut calls: FuturesUnordered<CallFuture<'_>> = FuturesUnordered::new();
            calls.push(Box::pin(async {
                (
                    "c1".to_string(),
                    host.call_tool("slow", JsonObject::new(), "c1")
                        .await
                        .expect("slow call"),
                )
            }));
            calls.push(Box::pin(async {
                (
                    "c2".to_string(),
                    host.call_tool("fast", JsonObject::new(), "c2")
                        .await
                        .expect("fast call"),
                )
            }));
            let mut order = Vec::new();
            while let Some((key, _)) = calls.next().await {
                order.push(key.clone());
                if attempt == 0 && key == "c2" {
                    self.fast_delivered.notify_one();
                }
            }
            if attempt == 1 {
                host.call_tool("later", JsonObject::new(), "c3").await?;
                host.call_tool("later", JsonObject::new(), "c4").await?;
            }
            self.orders.lock().expect("orders").push(order.clone());
            if attempt == 0 {
                self.journaled.notify_one();
                self.hold.notified().await;
            }
            Ok(CodemodeOutcome::output(order.join(",")))
        }
    }

    struct GateTool {
        runs: Arc<AtomicUsize>,
        value: &'static str,
        started: Option<Arc<tokio::sync::Notify>>,
        release: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait]
    impl ToolExecute for GateTool {
        async fn execute(
            &self,
            _args: JsonObject,
            _api: &dyn ToolExecutionApi,
            _context: &Context,
        ) -> Result<ToolExecutionResult, ToolExecuteError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            if let Some(started) = &self.started {
                started.notify_one();
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            Ok(ToolExecutionResult {
                structured_output: Some(serde_json::json!({ "tool": self.value })),
                ..Default::default()
            })
        }
    }

    let slow_started = Arc::new(tokio::sync::Notify::new());
    let slow_release = Arc::new(tokio::sync::Notify::new());
    let fast_delivered = Arc::new(tokio::sync::Notify::new());
    let journaled = Arc::new(tokio::sync::Notify::new());
    let hold = Arc::new(tokio::sync::Notify::new());
    let slow_runs = Arc::new(AtomicUsize::new(0));
    let fast_runs = Arc::new(AtomicUsize::new(0));
    let later_runs = Arc::new(AtomicUsize::new(0));
    let orders = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sandbox = Arc::new(OrderedSandbox {
        attempts: AtomicUsize::new(0),
        orders: Arc::clone(&orders),
        fast_delivered: Arc::clone(&fast_delivered),
        journaled: Arc::clone(&journaled),
        hold,
    });
    let extensions = || {
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::clone(&sandbox) as Arc<dyn CodemodeSandbox>,
                ..Default::default()
            }),
            tool(
                "slow",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({})),
                Arc::new(GateTool {
                    runs: Arc::clone(&slow_runs),
                    value: "slow",
                    started: Some(Arc::clone(&slow_started)),
                    release: Some(Arc::clone(&slow_release)),
                }),
            ),
            tool(
                "fast",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({})),
                Arc::new(GateTool {
                    runs: Arc::clone(&fast_runs),
                    value: "fast",
                    started: None,
                    release: None,
                }),
            ),
            tool(
                "later",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({})),
                Arc::new(GateTool {
                    runs: Arc::clone(&later_runs),
                    value: "later",
                    started: None,
                    release: None,
                }),
            ),
        ])]
    };
    let models = || {
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "return 1" }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ])
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), extensions(), None, models()).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    slow_started.notified().await;
    fast_delivered.notified().await;
    slow_release.notify_one();
    journaled.notified().await;

    let parent = opened
        .harness
        .inspect(&context())
        .await
        .expect("inspect")
        .tasks
        .into_iter()
        .find(|task| task.record.kind == "pi.tool" && task.record.input["kind"] == "model")
        .expect("codemode parent")
        .record
        .id;
    let journal = opened
        .harness
        .session()
        .snapshot(&CODEMODE_JOURNAL_DOC, DocOwner::Task(parent), &context())
        .await
        .expect("journal")
        .expect("present");
    let journal: CodemodeJournal =
        serde_json::from_value(JsonValue::Object(journal)).expect("journal json");
    assert_eq!(
        journal
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["c2", "c1"]
    );

    let reopened = open_with_storage(
        Arc::clone(&storage),
        extensions(),
        None,
        test_models(vec![Script::Message(text_message(
            "done",
            rpi_ai::types::StopReason::Stop,
        ))]),
    )
    .await;
    let recovered = reopened
        .harness
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("present");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recovered.wait(&context()),
    )
    .await
    .expect("replay did not deadlock")
    .expect("replay settled");
    assert_eq!(
        orders.lock().expect("orders").as_slice(),
        &[
            vec!["c2".to_string(), "c1".to_string()],
            vec!["c2".to_string(), "c1".to_string()]
        ]
    );
    assert_eq!(slow_runs.load(Ordering::SeqCst), 1);
    assert_eq!(fast_runs.load(Ordering::SeqCst), 1);
    assert_eq!(later_runs.load(Ordering::SeqCst), 2);
    let results = tool_result_texts(&entries(&root_ready(&reopened.harness).await).await);
    assert!(
        results.iter().any(|text| text.contains("c2,c1")),
        "replay result: {results:?}"
    );
    assert!(
        !results.iter().any(|text| text.contains("different path")),
        "replay result: {results:?}"
    );
}

#[tokio::test]
async fn multiple_out_of_order_replay_waiters_are_rejected() {
    use futures::{StreamExt, stream::FuturesUnordered};
    use generation_support::open_with_storage;
    use rpi_durable::codemode::{
        CODEMODE_TOOL_NAME, CodemodeHost, CodemodeOutcome, CodemodeSandbox, CodemodeStart,
        CodemodeToolOptions, create_codemode_tool,
    };
    use rpi_durable::storage::MemoryStorage;
    use rpi_durable::types::Storage;
    use std::pin::Pin;

    struct SlowTool {
        runs: Arc<AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl ToolExecute for SlowTool {
        async fn execute(
            &self,
            _args: JsonObject,
            api: &dyn ToolExecutionApi,
            _context: &Context,
        ) -> Result<ToolExecutionResult, ToolExecuteError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if api.call_id().ends_with("/c1") {
                self.release.notified().await;
            }
            Ok(ToolExecutionResult {
                structured_output: Some(serde_json::json!({ "slow": true })),
                ..Default::default()
            })
        }
    }

    struct OutOfOrderSandbox {
        attempts: AtomicUsize,
        journaled: Arc<tokio::sync::Notify>,
        hold: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl CodemodeSandbox for OutOfOrderSandbox {
        fn fingerprint(&self) -> String {
            "multiple-out-of-order-waiters-1".to_string()
        }

        async fn run(
            &self,
            _start: CodemodeStart,
            host: &dyn CodemodeHost,
            _context: &Context,
        ) -> Result<CodemodeOutcome, String> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 1 {
                type CallFuture<'a> = Pin<
                    Box<dyn Future<Output = Result<NestedToolExecutionResult, String>> + Send + 'a>,
                >;
                let mut calls: FuturesUnordered<CallFuture<'_>> = FuturesUnordered::new();
                calls.push(Box::pin(host.call_tool("slow", JsonObject::new(), "c3")));
                calls.push(Box::pin(host.call_tool("slow", JsonObject::new(), "c1")));
                while let Some(result) = calls.next().await {
                    result?;
                }
                return Err("out-of-order replay was accepted".to_string());
            }
            host.call_tool("slow", JsonObject::new(), "c2").await?;
            host.call_tool("slow", JsonObject::new(), "c1").await?;
            host.call_tool("slow", JsonObject::new(), "c3").await?;
            self.journaled.notify_one();
            self.hold.notified().await;
            Ok(CodemodeOutcome::output("done"))
        }
    }

    let slow_started = Arc::new(tokio::sync::Notify::new());
    let slow_release = Arc::new(tokio::sync::Notify::new());
    let journaled = Arc::new(tokio::sync::Notify::new());
    let hold = Arc::new(tokio::sync::Notify::new());
    let slow_runs = Arc::new(AtomicUsize::new(0));
    let sandbox = Arc::new(OutOfOrderSandbox {
        attempts: AtomicUsize::new(0),
        journaled: Arc::clone(&journaled),
        hold,
    });
    let extensions = || {
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::clone(&sandbox) as Arc<dyn CodemodeSandbox>,
                ..Default::default()
            }),
            tool(
                "slow",
                Some(vec![ToolCaller::Tools]),
                Some(serde_json::json!({})),
                Arc::new(SlowTool {
                    runs: Arc::clone(&slow_runs),
                    started: Arc::clone(&slow_started),
                    release: Arc::clone(&slow_release),
                }),
            ),
        ])]
    };
    let models = || {
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "return 1" }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ])
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), extensions(), None, models()).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    slow_started.notified().await;
    slow_release.notify_one();
    journaled.notified().await;

    let reopened = open_with_storage(
        Arc::clone(&storage),
        extensions(),
        None,
        test_models(vec![Script::Message(text_message(
            "done",
            rpi_ai::types::StopReason::Stop,
        ))]),
    )
    .await;
    let recovered = reopened
        .harness
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("present");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recovered.wait(&context()),
    )
    .await
    .expect("multiple out-of-order waiters deadlocked")
    .expect("settled");
    let results = tool_result_texts(&entries(&root_ready(&reopened.harness).await).await);
    assert!(
        results.iter().any(|text| text.contains("out of order")),
        "out-of-order divergence was not reported: {results:?}"
    );
    assert_eq!(slow_runs.load(Ordering::SeqCst), 3);
}

async fn assert_crash_window_replay(underway: bool, replay_empty_calls: bool) {
    use generation_support::open_with_storage;
    use rpi_durable::codemode::{
        CODEMODE_JOURNAL_DOC, CODEMODE_TOOL_NAME, CodemodeHost, CodemodeJournal, CodemodeOutcome,
        CodemodeSandbox, CodemodeStart, CodemodeToolOptions, create_codemode_tool,
    };
    use rpi_durable::storage::MemoryStorage;
    use rpi_durable::types::Storage;

    struct WindowTool {
        attempts: Arc<AtomicUsize>,
        runs: Arc<AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
        hold: Arc<tokio::sync::Notify>,
        hold_keys: &'static [&'static str],
    }

    #[async_trait]
    impl ToolExecute for WindowTool {
        async fn execute(
            &self,
            _args: JsonObject,
            api: &dyn ToolExecutionApi,
            _context: &Context,
        ) -> Result<ToolExecutionResult, ToolExecuteError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if self.attempts.load(Ordering::SeqCst) == 1
                && self
                    .hold_keys
                    .iter()
                    .any(|key| api.call_id().ends_with(&format!("/{key}")))
            {
                self.hold.notified().await;
            }
            Ok(ToolExecutionResult {
                structured_output: Some(serde_json::json!({ "ok": true })),
                ..Default::default()
            })
        }
    }

    struct WindowSandbox {
        attempts: Arc<AtomicUsize>,
        underway: bool,
        replay_empty_calls: bool,
        tool_started: Arc<tokio::sync::Notify>,
        journaled: Arc<tokio::sync::Notify>,
        hold: Arc<tokio::sync::Notify>,
        replay_hold: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl CodemodeSandbox for WindowSandbox {
        fn fingerprint(&self) -> String {
            "crash-window-1".to_string()
        }

        async fn run(
            &self,
            _start: CodemodeStart,
            host: &dyn CodemodeHost,
            _context: &Context,
        ) -> Result<CodemodeOutcome, String> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 1 {
                if self.underway {
                    host.call_tool("inner", JsonObject::new(), "a").await?;
                    host.call_tool("inner", JsonObject::new(), "b").await?;
                    self.journaled.notify_one();
                    self.replay_hold.notified().await;
                } else if self.replay_empty_calls {
                    let (a, b) = tokio::join!(
                        host.call_tool("inner", JsonObject::new(), "a"),
                        host.call_tool("inner", JsonObject::new(), "b")
                    );
                    a?;
                    b?;
                }
                return Ok(CodemodeOutcome::output("done"));
            }
            if !self.underway && !self.replay_empty_calls {
                let next = host.call_tool("inner", JsonObject::new(), "a");
                tokio::pin!(next);
                let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut next).await;
                self.tool_started.notified().await;
                self.journaled.notify_one();
                self.hold.notified().await;
                next.await?;
                return Ok(CodemodeOutcome::output("done"));
            }
            if self.underway {
                host.call_tool("inner", JsonObject::new(), "a").await?;
            }
            let next = host.call_tool("inner", JsonObject::new(), "b");
            tokio::pin!(next);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut next).await;
            let last = host.call_tool(
                "inner",
                JsonObject::new(),
                if self.underway { "c" } else { "a" },
            );
            tokio::pin!(last);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut last).await;
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                self.tool_started.notified(),
            )
            .await
            .expect("nested tools started");
            self.journaled.notify_one();
            self.hold.notified().await;
            let (next, last) = tokio::join!(next, last);
            next?;
            last?;
            Ok(CodemodeOutcome::output("done"))
        }
    }

    let attempts = Arc::new(AtomicUsize::new(0));
    let tool_started = Arc::new(tokio::sync::Notify::new());
    let journaled = Arc::new(tokio::sync::Notify::new());
    let hold = Arc::new(tokio::sync::Notify::new());
    let replay_hold = Arc::new(tokio::sync::Notify::new());
    let immediate_runs = Arc::new(AtomicUsize::new(0));
    let held_runs = Arc::new(AtomicUsize::new(0));
    let sandbox = Arc::new(WindowSandbox {
        attempts: Arc::clone(&attempts),
        underway,
        replay_empty_calls,
        tool_started: Arc::clone(&tool_started),
        journaled: Arc::clone(&journaled),
        hold: Arc::clone(&hold),
        replay_hold: Arc::clone(&replay_hold),
    });
    let extensions = || {
        let immediate = tool(
            "inner",
            Some(vec![ToolCaller::Tools]),
            Some(serde_json::json!({})),
            Arc::new(WindowTool {
                attempts: Arc::clone(&attempts),
                runs: Arc::clone(&immediate_runs),
                started: Arc::clone(&tool_started),
                hold: Arc::clone(&hold),
                hold_keys: &["b", "c"],
            }),
        );
        let held = tool(
            "inner",
            Some(vec![ToolCaller::Tools]),
            Some(serde_json::json!({})),
            Arc::new(WindowTool {
                attempts: Arc::clone(&attempts),
                runs: Arc::clone(&held_runs),
                started: Arc::clone(&tool_started),
                hold: Arc::clone(&hold),
                hold_keys: &["a", "b"],
            }),
        );
        vec![extension(vec![
            create_codemode_tool(CodemodeToolOptions {
                sandbox: Arc::clone(&sandbox) as Arc<dyn CodemodeSandbox>,
                ..Default::default()
            }),
            if underway { immediate } else { held },
        ])]
    };
    let models = || {
        test_models(vec![
            Script::Message(tool_use_message(
                CODEMODE_TOOL_NAME,
                "c1",
                serde_json::json!({ "code": "return 1" }),
            )),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ])
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), extensions(), None, models()).await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    tokio::time::timeout(std::time::Duration::from_secs(2), journaled.notified())
        .await
        .expect("first run did not reach the crash window");

    let parent = opened
        .harness
        .inspect(&context())
        .await
        .expect("inspect")
        .tasks
        .into_iter()
        .find(|task| task.record.kind == "pi.tool" && task.record.input["kind"] == "model")
        .expect("codemode parent")
        .record
        .id;
    let journal = opened
        .harness
        .session()
        .snapshot(&CODEMODE_JOURNAL_DOC, DocOwner::Task(parent), &context())
        .await
        .expect("journal")
        .expect("present");
    let journal: CodemodeJournal =
        serde_json::from_value(JsonValue::Object(journal)).expect("journal json");
    if underway {
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].issued_after, Some(3));
    } else {
        assert!(journal.entries.is_empty());
        assert_eq!(
            journal.first_issued,
            Some(if replay_empty_calls { 2 } else { 1 })
        );
    }

    let reopened = open_with_storage(
        Arc::clone(&storage),
        extensions(),
        None,
        test_models(vec![Script::Message(text_message(
            "done",
            rpi_ai::types::StopReason::Stop,
        ))]),
    )
    .await;
    reopened.harness.resume().expect("resume");
    let recovered = reopened
        .harness
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("present");
    if underway {
        tokio::time::timeout(std::time::Duration::from_secs(2), journaled.notified())
            .await
            .expect("replay did not acknowledge the next call");
        let journal = reopened
            .harness
            .session()
            .snapshot(&CODEMODE_JOURNAL_DOC, DocOwner::Task(parent), &context())
            .await
            .expect("journal")
            .expect("present");
        let journal: CodemodeJournal =
            serde_json::from_value(JsonValue::Object(journal)).expect("journal json");
        assert_eq!(journal.entries[0].key, "a");
        assert_eq!(journal.entries[0].issued_after, Some(3));
        replay_hold.notify_one();
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recovered.wait(&context()),
    )
    .await
    .expect("crash-window replay deadlocked")
    .expect("settled");
    let parent_task = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reopened.harness.wait_for_task(parent, &context()),
    )
    .await
    .expect("codemode parent did not settle")
    .expect("codemode parent");
    let parent_state =
        serde_json::to_string(&parent_task.record.state).expect("codemode parent state");
    let results = tool_result_texts(&entries(&root_ready(&reopened.harness).await).await);
    let replay_output = format!("{parent_state}\n{}", results.join("\n"));
    if replay_empty_calls {
        assert!(
            replay_output.contains("done") && !replay_output.contains("different path"),
            "legitimate concurrent replay was rejected: {replay_output}"
        );
    } else {
        assert!(
            replay_output.contains("different path"),
            "divergent crash-window replay was accepted: attempts={} output={replay_output}",
            attempts.load(Ordering::SeqCst)
        );
    }
    assert_eq!(
        immediate_runs.load(Ordering::SeqCst),
        if underway { 3 } else { 0 },
        "attempts={} results={results:?}",
        attempts.load(Ordering::SeqCst)
    );
    assert_eq!(
        held_runs.load(Ordering::SeqCst),
        if underway {
            0
        } else if replay_empty_calls {
            2
        } else {
            1
        }
    );
}

#[tokio::test]
async fn empty_journal_divergence_is_rejected() {
    assert_crash_window_replay(false, false).await;
}

#[tokio::test]
async fn concurrent_empty_journal_replay_is_accepted() {
    assert_crash_window_replay(false, true).await;
}

#[tokio::test]
async fn underway_next_call_divergence_is_rejected() {
    assert_crash_window_replay(true, false).await;
}

struct ChildDefinition;

#[async_trait]
impl rpi_durable::harness::TaskDefinition for ChildDefinition {
    fn name(&self) -> &str {
        "test.child"
    }

    fn version(&self) -> u64 {
        1
    }

    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "done" })
    }

    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &Context,
    ) -> rpi_durable::session::SessionResult<()> {
        runtime
            .commit(
                Arc::new(|_tx, _current| {
                    Box::pin(async move {
                        Ok(Some(rpi_durable::harness::NextTaskState::Terminal {
                            outcome: rpi_durable::types::TaskOutcome::Completed {
                                result: JsonValue::Null,
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }
}

struct CreateChildTool {
    definition: rpi_durable::harness::AnyTask,
    created: Arc<std::sync::Mutex<Vec<TaskId>>>,
    abandon_on_restart: Option<bool>,
    conversation: Option<rpi_durable::ids::ConversationId>,
}

#[async_trait]
impl ToolExecute for CreateChildTool {
    async fn execute(
        &self,
        _args: JsonObject,
        api: &dyn ToolExecutionApi,
        context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        let ownership = match self.conversation {
            Some(conversation) => rpi_durable::session::TaskOwnership::Conversation(conversation),
            None => rpi_durable::session::TaskOwnership::Task(api.task_id()),
        };
        let id = api
            .create_task(
                Arc::clone(&self.definition),
                JsonValue::Null,
                ownership,
                false,
                self.abandon_on_restart,
                context,
            )
            .await?;
        self.created.lock().expect("created").push(id);
        Ok(ToolExecutionResult::default())
    }
}

/// FR-I R2: tool-created children inherit abandonment only from an unsafe
/// caller, and an explicit option always takes precedence.
#[tokio::test]
async fn tool_created_tasks_apply_the_abandonment_default_and_override() {
    for (replay, explicit, expected) in [
        (ToolReplay::Unsafe, None, true),
        (ToolReplay::Safe, None, false),
        (ToolReplay::Unsafe, Some(false), false),
        (ToolReplay::Safe, Some(true), true),
    ] {
        let definition = Arc::new(ChildDefinition) as rpi_durable::harness::AnyTask;
        let created = Arc::new(std::sync::Mutex::new(Vec::new()));
        let creator = tool(
            "creator",
            None,
            None,
            Arc::new(CreateChildTool {
                definition: Arc::clone(&definition),
                created: Arc::clone(&created),
                abandon_on_restart: explicit,
                conversation: None,
            }),
        );
        let mut creator = creator;
        creator.replay = replay;
        let opened = open(
            vec![Extension {
                name: "nested".to_string(),
                tools: vec![creator],
                tasks: vec![definition],
                ..Extension::default()
            }],
            None,
            test_models(vec![
                Script::Message(tool_use_message("creator", "c1", serde_json::json!({}))),
                Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
            ]),
        )
        .await;
        let conversation = root_ready(&opened.harness).await;
        let submission = submit_input(&conversation, "hi").await;
        submission.wait(&context()).await.expect("wait");
        let child = created.lock().expect("created")[0];
        let record = opened
            .harness
            .get_task(child, &context())
            .await
            .expect("child")
            .expect("record");
        assert_eq!(record.abandon_on_restart, expected);
        assert!(record.owner.is_some(), "the child is task-owned");
    }
}

/// Conversation-owned children do not inherit the unsafe caller's abandonment
/// default, because their lifetime is not an in-memory wait owned by the tool.
#[tokio::test]
async fn conversation_owned_children_do_not_inherit_abandonment() {
    let definition = Arc::new(ChildDefinition) as rpi_durable::harness::AnyTask;
    let created = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut creator = tool(
        "creator",
        None,
        None,
        Arc::new(CreateChildTool {
            definition: Arc::clone(&definition),
            created: Arc::clone(&created),
            abandon_on_restart: None,
            conversation: Some(rpi_durable::ids::ROOT_CONVERSATION_ID),
        }),
    );
    creator.replay = ToolReplay::Unsafe;
    let opened = open(
        vec![Extension {
            name: "nested".to_string(),
            tools: vec![creator],
            tasks: vec![definition],
            ..Extension::default()
        }],
        None,
        test_models(vec![
            Script::Message(tool_use_message("creator", "c1", serde_json::json!({}))),
            Script::Message(text_message("done", rpi_ai::types::StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    let child = created.lock().expect("created")[0];
    let record = opened
        .harness
        .get_task(child, &context())
        .await
        .expect("child")
        .expect("record");
    assert!(!record.abandon_on_restart);
    assert_eq!(record.owner, None);
}

/// FR-A R3/R4: a second-level nested slot keeps its complete direct parent
/// call ID and parent task ID until the outermost call settles.
#[tokio::test]
async fn deeply_nested_slots_keep_the_direct_parent_call_id() {
    struct LeafTool;
    #[async_trait]
    impl ToolExecute for LeafTool {
        async fn execute(
            &self,
            _args: JsonObject,
            _api: &dyn ToolExecutionApi,
            _context: &Context,
        ) -> Result<ToolExecutionResult, ToolExecuteError> {
            Ok(ToolExecutionResult::default())
        }
    }

    struct MiddleTool {
        leaf_done: Arc<tokio::sync::Notify>,
        finish: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl ToolExecute for MiddleTool {
        async fn execute(
            &self,
            _args: JsonObject,
            api: &dyn ToolExecutionApi,
            context: &Context,
        ) -> Result<ToolExecutionResult, ToolExecuteError> {
            api.execute_tool("leaf", JsonObject::new(), context, Default::default())
                .await?;
            self.leaf_done.notify_one();
            self.finish.notified().await;
            Ok(ToolExecutionResult::default())
        }
    }

    struct OuterTool;
    #[async_trait]
    impl ToolExecute for OuterTool {
        async fn execute(
            &self,
            _args: JsonObject,
            api: &dyn ToolExecutionApi,
            context: &Context,
        ) -> Result<ToolExecutionResult, ToolExecuteError> {
            api.execute_tool("middle", JsonObject::new(), context, Default::default())
                .await?;
            Ok(ToolExecutionResult::default())
        }
    }

    let middle_done = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let opened = open(
        vec![extension(vec![
            tool("outer", None, None, Arc::new(OuterTool)),
            tool(
                "middle",
                None,
                None,
                Arc::new(MiddleTool {
                    leaf_done: Arc::clone(&middle_done),
                    finish: Arc::clone(&finish),
                }),
            ),
            tool("leaf", None, None, Arc::new(LeafTool)),
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
    middle_done.notified().await;
    let live = opened
        .harness
        .session()
        .snapshot(
            &rpi_durable::harness::LIVE_DOC,
            DocOwner::Conversation(conversation.id()),
            &context(),
        )
        .await
        .expect("live")
        .expect("present");
    let live: rpi_durable::harness::LiveState =
        serde_json::from_value(JsonValue::Object(live)).expect("live state");
    let nested = live.nested_tools.expect("nested slots");
    let middle = nested
        .iter()
        .find(|slot| slot.progress.call_id == "c1/1")
        .expect("middle slot");
    let leaf = nested
        .iter()
        .find(|slot| slot.progress.call_id == "c1/1/1")
        .expect("leaf slot");
    assert_eq!(leaf.parent_call_id, "c1/1");
    assert_eq!(leaf.parent_task_id, middle.task_id);
    finish.notify_one();
    submission.wait(&context()).await.expect("wait");
}
