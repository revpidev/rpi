mod generation_support;
mod session_support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use generation_support::{Script, context, entries, root_ready, submit_input, test_models};
use rpi_ai::types::{AssistantContent, StopReason, Tool as AiTool, ToolResultContent};
use rpi_durable::chord::context::Context;
use rpi_durable::errors::StorageError;
use rpi_durable::harness::NextTaskState;
use rpi_durable::harness::compaction::create_compaction;
use rpi_durable::harness::types::ConversationRetryPolicyPatch;
use rpi_durable::harness::{
    AfterFailedRun, AgentChange, CompactionReason, ContinueSubmissionDraft, Extension,
    GenerationHooks, HarnessSettings, HookApi, HookHandlers, HookRegistration,
    InputSubmissionDraft, LIVE_DOC, LiveState, ProviderRequest, RunEndTask, RunEndTasks,
    RunSummary, SubmissionDraft, TaskDefinition, TaskRuntime, ToolExecute, ToolExecuteError,
    ToolExecutionApi, ToolExecutionMode, ToolExecutionResult, ToolRegistration, ToolReplay,
    WhenBusy, WhenIdle,
};
use rpi_durable::session::{DocOwner, SessionResult};
use rpi_durable::types::{
    ContinueSubmissionStatus, InputSubmissionStatus, Storage, StorageExt, SubmissionRecord,
    TaskOutcome, TaskQuery, TaskState, TaskStatus,
};

fn answer(text: &str) -> Script {
    Script::Message(generation_support::text_message(text, StopReason::Stop))
}

fn tool_call(name: &str, id: &str) -> Script {
    Script::Message(generation_support::tool_use_message(
        name,
        id,
        serde_json::json!({}),
    ))
}

fn error(text: &str) -> Script {
    Script::Message(generation_support::error_message("fatal", text))
}

fn tool_round(calls: &[(&str, &str)]) -> Script {
    let (first_name, first_id) = calls[0];
    let mut message =
        generation_support::tool_use_message(first_name, first_id, serde_json::json!({}));
    for (name, id) in &calls[1..] {
        message
            .content
            .push(AssistantContent::ToolCall(rpi_ai::types::ToolCall {
                id: (*id).to_string(),
                name: (*name).to_string(),
                arguments: serde_json::Map::new(),
                arguments_error: None,
                namespace: None,
                thought_signature: None,
            }));
    }
    Script::Message(message)
}

fn status(record: &SubmissionRecord) -> Option<&InputSubmissionStatus> {
    match record {
        SubmissionRecord::Input { status, .. } => Some(status),
        _ => None,
    }
}

fn has_assistant_text(entries: &[rpi_durable::types::EntryRecord], expected: &str) -> bool {
    entries.iter().any(|entry| {
        entry.model.iter().flatten().any(|message| match message {
            rpi_ai::types::Message::Assistant(assistant) => assistant.content.iter().any(
                |content| matches!(content, AssistantContent::Text(text) if text.text == expected),
            ),
            _ => false,
        })
    })
}

fn has_tool_text(entries: &[rpi_durable::types::EntryRecord], expected: &str) -> bool {
    entries.iter().any(|entry| {
        entry.model.iter().flatten().any(|message| match message {
            rpi_ai::types::Message::ToolResult(result) => result.content.iter().any(
                |content| matches!(content, ToolResultContent::Text(text) if text.text.contains(expected)),
            ),
            _ => false,
        })
    })
}

struct PayloadHook(&'static str);

#[async_trait]
impl GenerationHooks for PayloadHook {
    async fn before_provider_request(
        &self,
        mut request: ProviderRequest,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<ProviderRequest> {
        if self.0 == "panic" {
            panic!("payload hook failure fixture");
        }
        if !request.payload["chain"].is_array() {
            request.payload["chain"] = serde_json::json!([]);
        }
        request.payload["chain"]
            .as_array_mut()
            .expect("chain array")
            .push(serde_json::json!(self.0));
        Some(request)
    }

    async fn before_request(
        &self,
        _request: rpi_durable::harness::RequestMessages,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<rpi_durable::harness::RequestMessages> {
        None
    }

    async fn after_response(
        &self,
        _message: &rpi_ai::types::AssistantMessage,
        _api: &dyn HookApi,
        _context: &Context,
    ) {
    }

    async fn on_yield(
        &self,
        _answer: &rpi_ai::types::AssistantMessage,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<rpi_durable::harness::ContinueInput> {
        None
    }

    async fn after_tools(
        &self,
        _assistant: rpi_durable::ids::EntryId,
        _results: &[rpi_durable::ids::EntryId],
        _api: &dyn HookApi,
        _context: &Context,
    ) {
    }
}

fn payload_extension() -> Extension {
    Extension {
        name: "payload".to_string(),
        hooks: vec![
            HookRegistration {
                task: "pi.generation".to_string(),
                handlers: HookHandlers::Generation(Arc::new(PayloadHook("first"))),
            },
            HookRegistration {
                task: "pi.generation".to_string(),
                handlers: HookHandlers::Generation(Arc::new(PayloadHook("second"))),
            },
            HookRegistration {
                task: "pi.generation".to_string(),
                handlers: HookHandlers::Generation(Arc::new(PayloadHook("panic"))),
            },
        ],
        ..Extension::default()
    }
}

#[tokio::test]
async fn provider_payload_replaces_in_order_and_compaction_does_not_call() {
    let (models, payloads) =
        generation_support::test_models_with_payload_log(vec![answer("answer"), answer("summary")]);
    let opened = generation_support::open(vec![payload_extension()], None, models).await;
    let conversation = root_ready(&opened.harness).await;
    let conversation_id = conversation.id();
    submit_input(&conversation, "hi")
        .await
        .wait(&context())
        .await
        .expect("answer");
    let task = opened
        .harness
        .session()
        .commit(
            rpi_durable::session::commit_fn(move |tx| {
                Box::pin(async move {
                    create_compaction(tx, conversation_id, CompactionReason::Manual, None, None)
                        .await
                })
            }),
            &context(),
        )
        .await
        .expect("compaction");
    opened
        .harness
        .wait_for_task(task, &context())
        .await
        .expect("summary");
    let payloads = payloads.lock().expect("payloads");
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["chain"], serde_json::json!(["first", "second"]));
}

struct AfterRunHook {
    once: AtomicBool,
    child: Arc<AfterTask>,
    summaries: Arc<Mutex<Vec<RunSummary>>>,
}

#[async_trait]
impl GenerationHooks for AfterRunHook {
    async fn after_run(
        &self,
        run: RunSummary,
        api: &dyn HookApi,
        context: &Context,
    ) -> Option<RunEndTasks> {
        self.summaries.lock().expect("summaries").push(run.clone());
        if self.once.swap(true, Ordering::SeqCst) {
            return None;
        }
        let history = api
            .entries(
                rpi_durable::types::EntryQuery {
                    conversation_id: api.conversation_id(),
                    min_entry_id: run.first,
                    max_entry_id: Some(run.last),
                    order: Some(rpi_durable::types::ScanOrder::Ascending),
                },
                100,
                None,
                context,
            )
            .await
            .expect("raw run history");
        assert!(history.items.iter().any(|entry| entry.id == run.answer));
        Some(RunEndTasks {
            tasks: vec![RunEndTask {
                task: Arc::clone(&self.child) as rpi_durable::harness::AnyTask,
                input: serde_json::json!({}),
            }],
        })
    }

    async fn before_request(
        &self,
        _request: rpi_durable::harness::RequestMessages,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<rpi_durable::harness::RequestMessages> {
        None
    }

    async fn after_response(
        &self,
        _message: &rpi_ai::types::AssistantMessage,
        _api: &dyn HookApi,
        _context: &Context,
    ) {
    }

    async fn on_yield(
        &self,
        _answer: &rpi_ai::types::AssistantMessage,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<rpi_durable::harness::ContinueInput> {
        None
    }

    async fn after_tools(
        &self,
        _assistant: rpi_durable::ids::EntryId,
        _results: &[rpi_durable::ids::EntryId],
        _api: &dyn HookApi,
        _context: &Context,
    ) {
    }
}

struct AfterTask {
    started: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl TaskDefinition for AfterTask {
    fn name(&self) -> &str {
        "test.after-run"
    }

    fn version(&self) -> u64 {
        1
    }

    fn initial(&self) -> serde_json::Value {
        serde_json::json!({ "phase": "work" })
    }

    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn TaskRuntime,
        context: &Context,
    ) -> SessionResult<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.release.notified().await;
        let change: rpi_durable::harness::TaskCommitFn = Arc::new(move |_tx, _current| {
            Box::pin(async move {
                Ok(Some(NextTaskState::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: serde_json::json!({ "done": true }),
                    },
                }))
            })
        });
        runtime.commit(change, context).await
    }
}

#[tokio::test]
async fn after_run_children_hold_the_final_boundary_and_expose_raw_history() {
    let child = Arc::new(AfterTask {
        started: Arc::new(AtomicUsize::new(0)),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let summaries = Arc::new(Mutex::new(Vec::new()));
    let extension = Extension {
        name: "after-run".to_string(),
        tasks: vec![Arc::clone(&child) as rpi_durable::harness::AnyTask],
        hooks: vec![HookRegistration {
            task: "pi.generation".to_string(),
            handlers: HookHandlers::Generation(Arc::new(AfterRunHook {
                once: AtomicBool::new(false),
                child: Arc::clone(&child),
                summaries: Arc::clone(&summaries),
            })),
        }],
        ..Extension::default()
    };
    let opened = generation_support::open(
        vec![extension],
        None,
        test_models(vec![answer("first"), answer("followup")]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let running = submit_input(&conversation, "hi").await;
    assert_eq!(
        running
            .wait(&context())
            .await
            .expect("first answer")
            .status(),
        "done"
    );
    for _ in 0..200 {
        if child.started.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(child.started.load(Ordering::SeqCst), 1);
    let followup = conversation
        .submit(
            SubmissionDraft::Input(InputSubmissionDraft {
                request_id: None,
                content: rpi_ai::types::UserContent::Text("next".to_string()),
                when_busy: Some(WhenBusy::FollowUp),
                when_idle: None,
                entry: None,
            }),
            &context(),
        )
        .await
        .expect("queued followup");
    assert_eq!(
        followup.status(&context()).await.expect("status").status(),
        "queued"
    );
    child.release.notify_one();
    assert_eq!(
        followup.wait(&context()).await.expect("followup").status(),
        "done"
    );
    assert_eq!(child.started.load(Ordering::SeqCst), 1);
    let summaries = summaries.lock().expect("summaries");
    assert_eq!(summaries.len(), 2);
    assert_eq!(
        summaries[0].reason,
        rpi_durable::harness::RunEndReason::Answer
    );
    assert!(summaries[0].first < Some(summaries[0].last));
    assert_eq!(summaries[0].answer, summaries[0].last);
    assert!(summaries[0].first.is_some());
}

struct BlockingTool {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ToolExecute for BlockingTool {
    async fn execute(
        &self,
        _args: rpi_durable::chord::json::JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        self.started.notify_one();
        self.release.notified().await;
        api.output(b"tool output", None).expect("output");
        Ok(ToolExecutionResult::default())
    }
}

fn blocking_extension(
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> Extension {
    Extension {
        name: "blocking".to_string(),
        tools: vec![ToolRegistration {
            tool: AiTool {
                name: "block".to_string(),
                description: "block".to_string(),
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
                constrained_sampling: None,
            },
            replay: ToolReplay::Safe,
            execution_mode: None,
            prepare_arguments: None,
            output_limits: None,
            structured_output_schema: None,
            callers: None,
            deferred: false,
            execute: Arc::new(BlockingTool { started, release }),
        }],
        ..Extension::default()
    }
}

#[tokio::test]
async fn continue_rejects_busy_and_failed_run_items_continue_first() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let settings = HarnessSettings {
        after_failed_run: Some(AfterFailedRun::Keep),
        retry: Some(ConversationRetryPolicyPatch {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opened = generation_support::open(
        vec![blocking_extension(
            Arc::clone(&started),
            Arc::clone(&release),
        )],
        Some(settings),
        test_models(vec![
            tool_call("block", "c1"),
            error("fatal"),
            answer("recovered"),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let running = submit_input(&conversation, "hi").await;
    started.notified().await;
    let error = conversation
        .submit(
            SubmissionDraft::Continue(ContinueSubmissionDraft {
                request_id: None,
                when_busy: None,
                when_idle: None,
            }),
            &context(),
        )
        .await
        .err()
        .expect("busy continue");
    assert!(matches!(
        error,
        rpi_durable::SessionError::Storage(StorageError::ConversationBusy(_))
    ));
    let followup = conversation
        .submit(
            SubmissionDraft::Input(InputSubmissionDraft {
                request_id: None,
                content: rpi_ai::types::UserContent::Text("queued".to_string()),
                when_busy: Some(WhenBusy::FollowUp),
                when_idle: None,
                entry: None,
            }),
            &context(),
        )
        .await
        .expect("followup");
    release.notify_one();
    let failed =
        match tokio::time::timeout(std::time::Duration::from_secs(10), running.wait(&context()))
            .await
        {
            Ok(failed) => failed.expect("failed run"),
            Err(error) => {
                eprintln!("submission: {:?}", running.status(&context()).await);
                eprintln!("inspection: {:?}", opened.harness.inspect(&context()).await);
                eprintln!(
                    "live: {:?}",
                    opened
                        .harness
                        .session()
                        .snapshot(
                            &LIVE_DOC,
                            DocOwner::Conversation(conversation.id()),
                            &context(),
                        )
                        .await
                );
                panic!("failed run timeout: {error}");
            }
        };
    assert!(matches!(
        status(&failed),
        Some(InputSubmissionStatus::Unanswered { reason, .. }) if reason == "model_error"
    ));
    let continuation = conversation
        .submit(
            SubmissionDraft::Continue(ContinueSubmissionDraft {
                request_id: None,
                when_busy: None,
                when_idle: None,
            }),
            &context(),
        )
        .await
        .expect("continue");
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            followup.wait(&context()),
        )
        .await
        .expect("queued first timeout")
        .expect("queued first")
        .status(),
        "done"
    );
    let continued = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        continuation.wait(&context()),
    )
    .await
    .expect("continue timeout")
    .expect("continue done");
    assert!(matches!(
        continued,
        SubmissionRecord::Continue {
            status: ContinueSubmissionStatus::Done { .. },
            ..
        }
    ));
    let texts = entries(&conversation).await;
    assert!(has_assistant_text(&texts, "recovered"));
}

#[tokio::test]
async fn continue_after_an_answer_is_nothing_to_answer_without_a_request() {
    let (models, _, requests) =
        generation_support::test_models_with_recovery_log(vec![answer("done")]);
    let opened = generation_support::open(Vec::new(), None, models).await;
    let conversation = root_ready(&opened.harness).await;
    submit_input(&conversation, "hi")
        .await
        .wait(&context())
        .await
        .expect("answer");
    let continued = conversation
        .submit(
            SubmissionDraft::Continue(ContinueSubmissionDraft {
                request_id: None,
                when_busy: None,
                when_idle: None,
            }),
            &context(),
        )
        .await
        .expect("continue");
    let settled_record = continued.wait(&context()).await.expect("settled");
    assert!(matches!(
        settled_record,
        SubmissionRecord::Continue {
            status: ContinueSubmissionStatus::Unanswered { ref reason, .. },
            ..
        } if reason == "nothing_to_answer"
    ));
    assert_eq!(settled_record.status(), "unanswered");
    assert_eq!(requests.lock().expect("requests").len(), 1);
}

#[tokio::test]
async fn max_turns_finishes_tool_results_before_turn_limit() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (models, _, requests) =
        generation_support::test_models_with_recovery_log(vec![tool_call("block", "c1")]);
    let opened = generation_support::open(
        vec![blocking_extension(
            Arc::clone(&started),
            Arc::clone(&release),
        )],
        None,
        models,
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    conversation
        .configure(
            AgentChange {
                max_turns: Some(Some(1)),
                ..Default::default()
            },
            &context(),
        )
        .await
        .expect("maxTurns");
    let running = submit_input(&conversation, "hi").await;
    started.notified().await;
    release.notify_one();
    let settled = running.wait(&context()).await.expect("turn limit");
    assert!(matches!(
        status(&settled),
        Some(InputSubmissionStatus::Unanswered { reason, .. }) if reason == "turn_limit"
    ));
    assert_eq!(requests.lock().expect("requests").len(), 1);
    assert!(has_tool_text(&entries(&conversation).await, "tool output"));
}

#[tokio::test]
async fn unoffered_creates_a_pending_task_and_settles_tool_unavailable() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let settings = HarnessSettings {
        tool_execution: Some(ToolExecutionMode::Sequential),
        ..Default::default()
    };
    let opened = generation_support::open(
        vec![blocking_extension(
            Arc::clone(&started),
            Arc::clone(&release),
        )],
        Some(settings),
        test_models(vec![
            tool_round(&[("block", "c1"), ("missing", "c2")]),
            answer("done"),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let running = submit_input(&conversation, "hi").await;
    started.notified().await;
    let live = opened
        .harness
        .session()
        .snapshot(
            &LIVE_DOC,
            DocOwner::Conversation(conversation.id()),
            &context(),
        )
        .await
        .expect("live")
        .expect("present");
    let state: LiveState = serde_json::from_value(serde_json::Value::Object(live)).expect("state");
    let slots = state.tools.expect("slots");
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[1].unoffered, Some(true));
    assert_eq!(
        slots[1].progress.status,
        rpi_durable::harness::ToolSlotStatus::Pending
    );
    assert_eq!(slots[1].task_id, None);
    let generation = state.run.expect("run").task_id;
    let generation = opened
        .harness
        .get_task(generation, &context())
        .await
        .expect("record")
        .expect("generation");
    assert!(matches!(
        generation.state,
        TaskState::Waiting { ref checkpoint, .. } if checkpoint["pending"] == serde_json::json!(["c2"])
    ));
    release.notify_one();
    assert_eq!(
        running.wait(&context()).await.expect("done").status(),
        "done"
    );
    let tasks = opened
        .storage
        .scan_tasks(
            TaskQuery {
                conversation_id: Some(conversation.id()),
                kind: Some("pi.tool".to_string()),
                status: Some(TaskStatus::Terminal),
                ..Default::default()
            },
            100,
            None,
            &context(),
        )
        .await
        .expect("tool tasks");
    let task = tasks
        .items
        .iter()
        .find(|task| task.input["callId"] == "c2")
        .expect("unoffered task");
    assert_eq!(task.input["unoffered"], serde_json::json!(true));
    let transcript = entries(&conversation).await;
    assert!(has_tool_text(&transcript, "Tool missing is not available"));
    assert!(transcript.iter().any(|entry| {
        entry.data.as_ref().is_some_and(|data| {
            data["diagnostics"][0]["code"] == serde_json::json!("tool_unavailable")
        }) && entry.model.iter().flatten().any(|message| {
            matches!(message, rpi_ai::types::Message::ToolResult(result) if result.tool_call_id == "c2" && result.is_error)
        })
    }));
}

#[tokio::test]
async fn idle_reject_does_not_write() {
    let opened = generation_support::open(Vec::new(), None, test_models(Vec::new())).await;
    let conversation = root_ready(&opened.harness).await;
    let error = conversation
        .submit(
            SubmissionDraft::Input(InputSubmissionDraft {
                request_id: None,
                content: rpi_ai::types::UserContent::Text("no run".to_string()),
                when_busy: None,
                when_idle: Some(WhenIdle::Reject),
                entry: None,
            }),
            &context(),
        )
        .await
        .err()
        .expect("idle reject");
    assert!(matches!(
        error,
        rpi_durable::SessionError::Storage(StorageError::ConversationIdle(_))
    ));
    assert!(
        opened
            .harness
            .inspect(&context())
            .await
            .expect("inspect")
            .submissions
            .is_empty()
    );
}

#[tokio::test]
async fn close_ends_watchers_before_the_deferred_commit() {
    let opened = session_support::open_test_session();
    let conversation = session_support::create_conversation(&opened.session).await;
    session_support::commit(&opened.session, move |tx| {
        Box::pin(async move {
            tx.doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    let watch = opened
        .session
        .watch_doc(&LIVE_DOC, DocOwner::Conversation(conversation), &context())
        .await
        .expect("watch")
        .expect("present");
    let initial = opened.storage.commits.lock().expect("commits").len();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let storage = Arc::clone(&opened.storage);
    opened.session.subscribe_close(Arc::new(move || {
        captured
            .lock()
            .expect("seen")
            .push(storage.commits.lock().expect("commits").len());
    }));
    opened
        .session
        .commit_with_options(
            rpi_durable::session::commit_fn(move |tx| {
                Box::pin(async move {
                    let live = tx
                        .doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                        .await?;
                    live.write()?["generation"] = serde_json::json!({ "attempt": 1 });
                    Ok(())
                })
            }),
            &context(),
            None,
            rpi_durable::session::CommitOptions {
                persistence: Some(rpi_durable::session::CommitPersistence::Deferred {
                    within_ms: Some(60_000.0),
                }),
            },
        )
        .await
        .expect("deferred");
    opened.session.close(&context()).await.expect("close");
    assert_eq!(watch.closed().await, rpi_durable::session::WatchEnd::Ended);
    assert_eq!(seen.lock().expect("seen").as_slice(), &[initial]);
    assert_eq!(
        opened.storage.commits.lock().expect("commits").len(),
        initial + 1
    );
}

#[tokio::test]
async fn deferred_progress_coalesces_string_deltas_to_a_constant_operation_batch() {
    let opened = session_support::open_test_session();
    let conversation = session_support::create_conversation(&opened.session).await;
    session_support::commit(&opened.session, move |tx| {
        Box::pin(async move {
            tx.doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    let mut expected = String::new();
    for value in 0..256u32 {
        expected.push_str(&format!(
            "{}{}",
            "x".repeat(15),
            char::from_u32(0x10000 + value).expect("chunk")
        ));
        let message = expected.clone();
        opened
            .session
            .commit_with_options(
                rpi_durable::session::commit_fn(move |tx| {
                    let message = message.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                            .await?;
                        live.write()?["generation"] = serde_json::json!({
                            "attempt": 1,
                            "message": message,
                        });
                        Ok(())
                    })
                }),
                &context(),
                None,
                rpi_durable::session::CommitOptions {
                    persistence: Some(rpi_durable::session::CommitPersistence::Deferred {
                        within_ms: Some(60_000.0),
                    }),
                },
            )
            .await
            .expect("deferred");
    }
    let before_close = opened.storage.commits.lock().expect("commits").len();
    opened.session.close(&context()).await.expect("close");
    let writes = opened.storage.last_commit().expect("deferred commit");
    let operations = writes.iter().find_map(|write| match write {
        rpi_durable::types::StorageWrite::DocumentChange {
            content: rpi_durable::types::DocumentContent::Delta { ops, .. },
            ..
        } => Some(ops),
        _ => None,
    });
    let operations = operations.expect("coalesced delta");
    assert_eq!(operations.len(), 2, "{operations:?}");
    assert!(matches!(
        operations[0],
        rpi_durable::chord::delta::Op::Set { .. }
    ));
    assert!(
        matches!(
            &operations[1],
            rpi_durable::chord::delta::Op::Append { value, .. } if value.as_str() == &expected[19..]
        ),
        "{operations:?}"
    );
    assert_eq!(
        opened.storage.commits.lock().expect("commits").len(),
        before_close + 1
    );
}

#[tokio::test]
async fn continue_after_turn_limit_preserves_the_failed_run_first_entry() {
    let child = Arc::new(AfterTask {
        started: Arc::new(AtomicUsize::new(0)),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let summaries = Arc::new(Mutex::new(Vec::new()));
    let after_run = Extension {
        name: "after-run".to_string(),
        tasks: vec![Arc::clone(&child) as rpi_durable::harness::AnyTask],
        hooks: vec![HookRegistration {
            task: "pi.generation".to_string(),
            handlers: HookHandlers::Generation(Arc::new(AfterRunHook {
                once: AtomicBool::new(false),
                child: Arc::clone(&child),
                summaries: Arc::clone(&summaries),
            })),
        }],
        ..Extension::default()
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let opened = generation_support::open(
        vec![
            after_run,
            blocking_extension(Arc::clone(&started), Arc::clone(&release)),
        ],
        None,
        test_models(vec![tool_call("block", "c1"), answer("recovered")]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    conversation
        .configure(
            AgentChange {
                max_turns: Some(Some(1)),
                ..Default::default()
            },
            &context(),
        )
        .await
        .expect("maxTurns");
    let running = submit_input(&conversation, "go").await;
    started.notified().await;
    release.notify_one();
    assert!(matches!(
        running.wait(&context()).await.expect("failed run"),
        SubmissionRecord::Input {
            status: InputSubmissionStatus::Unanswered { ref reason, .. },
            ..
        } if reason == "turn_limit"
    ));
    let live = opened
        .harness
        .session()
        .snapshot(
            &LIVE_DOC,
            DocOwner::Conversation(conversation.id()),
            &context(),
        )
        .await
        .expect("live")
        .expect("present");
    let state: LiveState = serde_json::from_value(serde_json::Value::Object(live)).expect("state");
    let first = state.run_failure.expect("failure").first.expect("first");
    let continuation = conversation
        .submit(
            SubmissionDraft::Continue(ContinueSubmissionDraft {
                request_id: None,
                when_busy: None,
                when_idle: None,
            }),
            &context(),
        )
        .await
        .expect("continue");
    continuation.wait(&context()).await.expect("continued");
    child.release.notify_one();
    if tokio::time::timeout(
        std::time::Duration::from_secs(10),
        conversation.wait_for_idle(&context()),
    )
    .await
    .is_err()
    {
        eprintln!("inspection: {:?}", opened.harness.inspect(&context()).await);
        panic!("continue afterRun idle timeout");
    }
    let summaries = summaries.lock().expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].first, Some(first));
    assert_eq!(summaries[0].answer, summaries[0].last);
}

struct ReplayHook {
    asked: Mutex<Vec<RunSummary>>,
    started: tokio::sync::Notify,
    gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    child: Arc<AfterTask>,
}

#[async_trait]
impl GenerationHooks for ReplayHook {
    async fn after_run(
        &self,
        run: RunSummary,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<RunEndTasks> {
        self.asked.lock().expect("asked").push(run);
        self.started.notify_one();
        let gate = self.gate.lock().expect("gate").take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Some(RunEndTasks {
            tasks: vec![RunEndTask {
                task: Arc::clone(&self.child) as rpi_durable::harness::AnyTask,
                input: serde_json::json!({}),
            }],
        })
    }

    async fn before_request(
        &self,
        _request: rpi_durable::harness::RequestMessages,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<rpi_durable::harness::RequestMessages> {
        None
    }

    async fn after_response(
        &self,
        _message: &rpi_ai::types::AssistantMessage,
        _api: &dyn HookApi,
        _context: &Context,
    ) {
    }

    async fn on_yield(
        &self,
        _answer: &rpi_ai::types::AssistantMessage,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<rpi_durable::harness::ContinueInput> {
        None
    }

    async fn after_tools(
        &self,
        _assistant: rpi_durable::ids::EntryId,
        _results: &[rpi_durable::ids::EntryId],
        _api: &dyn HookApi,
        _context: &Context,
    ) {
    }
}

#[tokio::test]
async fn after_run_replays_finish_once_and_drains_the_existing_child() {
    let storage = Arc::new(session_support::ControlledStorage::new());
    storage.keep_open_on_close();
    let child = Arc::new(AfterTask {
        started: Arc::new(AtomicUsize::new(0)),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let gate = Arc::new(tokio::sync::Notify::new());
    let hook = Arc::new(ReplayHook {
        asked: Mutex::new(Vec::new()),
        started: tokio::sync::Notify::new(),
        gate: Mutex::new(Some(Arc::clone(&gate))),
        child: Arc::clone(&child),
    });
    let extension = Extension {
        name: "after-run".to_string(),
        tasks: vec![Arc::clone(&child) as rpi_durable::harness::AnyTask],
        hooks: vec![HookRegistration {
            task: "pi.generation".to_string(),
            handlers: HookHandlers::Generation(Arc::clone(&hook) as Arc<dyn GenerationHooks>),
        }],
        ..Extension::default()
    };
    let first = generation_support::open_with_storage(
        Arc::clone(&storage) as Arc<dyn rpi_durable::types::Storage>,
        vec![extension.clone()],
        None,
        test_models(vec![answer("done")]),
    )
    .await;
    let conversation = root_ready(&first.harness).await;
    submit_input(&conversation, "go")
        .await
        .wait(&context())
        .await
        .expect("answer");
    hook.started.notified().await;
    storage.fail_next_commit(StorageError::message("simulated finish crash"));
    gate.notify_one();
    let _ = first.harness.session().closed().await;

    let second = generation_support::open_with_storage(
        Arc::clone(&storage) as Arc<dyn rpi_durable::types::Storage>,
        vec![extension.clone()],
        None,
        test_models(Vec::new()),
    )
    .await;
    second.harness.resume().expect("resume finish");
    for _ in 0..200 {
        if child.started.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(child.started.load(Ordering::SeqCst), 1);
    storage.fail_next_commit(StorageError::message("simulated drain crash"));
    child.release.notify_one();
    let _ = second.harness.session().closed().await;

    let third = generation_support::open_with_storage(
        Arc::clone(&storage) as Arc<dyn rpi_durable::types::Storage>,
        vec![extension],
        None,
        test_models(Vec::new()),
    )
    .await;
    child.release.notify_one();
    third
        .harness
        .root(&context(), Default::default())
        .await
        .expect("root")
        .wait_for_idle(&context())
        .await
        .expect("drain");
    assert_eq!(hook.asked.lock().expect("asked").len(), 2);
    assert_eq!(child.started.load(Ordering::SeqCst), 2);
    let tasks = storage
        .scan_tasks(
            TaskQuery {
                kind: Some("test.after-run".to_string()),
                status: Some(TaskStatus::Terminal),
                ..Default::default()
            },
            10,
            None,
            &context(),
        )
        .await
        .expect("tasks");
    assert_eq!(tasks.items.len(), 1);
    third.harness.close(&context()).await.expect("close");
}

#[tokio::test]
async fn dropping_a_session_discards_unflushed_deferred_progress() {
    let opened = session_support::open_test_session();
    let conversation = session_support::create_conversation(&opened.session).await;
    session_support::commit(&opened.session, move |tx| {
        Box::pin(async move {
            let live = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                .await?;
            live.write()?["generation"] = serde_json::json!({
                "attempt": 1,
                "message": "base",
            });
            Ok(())
        })
    })
    .await;
    opened
        .session
        .commit_with_options(
            rpi_durable::session::commit_fn(move |tx| {
                Box::pin(async move {
                    let live = tx
                        .doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                        .await?;
                    live.write()?["generation"]["message"] =
                        serde_json::json!("base plus held progress");
                    Ok(())
                })
            }),
            &context(),
            None,
            rpi_durable::session::CommitOptions {
                persistence: Some(rpi_durable::session::CommitPersistence::Deferred {
                    within_ms: Some(60_000.0),
                }),
            },
        )
        .await
        .expect("deferred");
    let commits = opened.storage.commits.lock().expect("commits").len();
    drop(opened.session);
    let reopened = rpi_durable::session::create_session(
        Arc::clone(&opened.storage) as Arc<dyn rpi_durable::types::Storage>,
        None,
    );
    let value = reopened
        .snapshot(&LIVE_DOC, DocOwner::Conversation(conversation), &context())
        .await
        .expect("snapshot")
        .expect("present");
    assert_eq!(value["generation"]["message"], serde_json::json!("base"));
    assert_eq!(
        opened.storage.commits.lock().expect("commits").len(),
        commits
    );
    reopened.close(&context()).await.expect("close");
}

#[derive(Default)]
struct InspectToolHooks {
    codes: Mutex<Vec<String>>,
    arguments: Mutex<Vec<rpi_durable::chord::json::JsonObject>>,
}

#[async_trait]
impl rpi_durable::harness::ToolHooks for InspectToolHooks {
    async fn before_tool(
        &self,
        call: &rpi_durable::harness::ToolHookCall,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<rpi_durable::harness::BeforeToolResult> {
        (call.call.name == "blocked").then(|| rpi_durable::harness::BeforeToolResult {
            arguments: None,
            block: Some("policy".to_string()),
        })
    }

    async fn after_tool(
        &self,
        call: &rpi_durable::harness::ToolHookCall,
        mut result: ToolExecutionResult,
        _api: &dyn HookApi,
        _context: &Context,
    ) -> Option<ToolExecutionResult> {
        self.arguments
            .lock()
            .expect("arguments")
            .push(call.call.arguments.clone());
        let code = result
            .diagnostics
            .as_ref()
            .and_then(|diagnostics| diagnostics.first())
            .and_then(|diagnostic| diagnostic.code.clone())
            .unwrap_or_else(|| "none".to_string());
        self.codes.lock().expect("codes").push(code.clone());
        result.output = Some(vec![ToolResultContent::Text(rpi_ai::types::TextContent {
            text: format!("hook:{code}"),
            text_signature: None,
        })]);
        result.structured_output = Some(serde_json::json!({ "mustBeCleared": true }));
        Some(result)
    }
}

struct CountingExecute(Arc<AtomicUsize>);

#[async_trait]
impl ToolExecute for CountingExecute {
    async fn execute(
        &self,
        _args: rpi_durable::chord::json::JsonObject,
        _api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolExecutionResult::default())
    }
}

fn registered_tool(name: &str, required: bool, execute: Arc<AtomicUsize>) -> ToolRegistration {
    ToolRegistration {
        tool: AiTool {
            name: name.to_string(),
            description: name.to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "required": required.then_some(vec!["n"]).unwrap_or_default(),
            }),
            constrained_sampling: None,
        },
        replay: ToolReplay::Safe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema: None,
        callers: None,
        deferred: false,
        execute: Arc::new(CountingExecute(execute)),
    }
}

#[tokio::test]
async fn after_tool_processes_unavailable_invalid_and_blocked_with_raw_arguments() {
    let hooks = Arc::new(InspectToolHooks::default());
    let executions = Arc::new(AtomicUsize::new(0));
    let opened = generation_support::open(
        vec![Extension {
            name: "inspect".to_string(),
            tools: vec![
                registered_tool("bad", true, Arc::clone(&executions)),
                registered_tool("blocked", false, Arc::clone(&executions)),
            ],
            hooks: vec![HookRegistration {
                task: "pi.tool".to_string(),
                handlers: HookHandlers::Tool(
                    Arc::clone(&hooks) as Arc<dyn rpi_durable::harness::ToolHooks>
                ),
            }],
            ..Extension::default()
        }],
        None,
        test_models(vec![
            tool_round(&[("missing", "c1"), ("bad", "c2"), ("blocked", "c3")]),
            answer("done"),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    assert_eq!(
        submit_input(&conversation, "go")
            .await
            .wait(&context())
            .await
            .expect("done")
            .status(),
        "done"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let mut codes = hooks.codes.lock().expect("codes").clone();
    codes.sort();
    assert_eq!(codes, ["blocked", "invalid_arguments", "tool_unavailable"]);
    assert!(
        hooks
            .arguments
            .lock()
            .expect("arguments")
            .iter()
            .all(|arguments| arguments.is_empty())
    );
    let transcript = entries(&conversation).await;
    for code in ["blocked", "invalid_arguments", "tool_unavailable"] {
        assert!(
            has_tool_text(&transcript, &format!("hook:{code}")),
            "missing {code}"
        );
    }
}

struct CancellableTool;

#[async_trait]
impl ToolExecute for CancellableTool {
    async fn execute(
        &self,
        _args: rpi_durable::chord::json::JsonObject,
        _api: &dyn ToolExecutionApi,
        context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        context
            .abort_signal()
            .expect("task signal")
            .cancelled()
            .await;
        Ok(ToolExecutionResult::default())
    }
}

fn cancellable_extension(hooks: Arc<InspectToolHooks>) -> Extension {
    let executions = Arc::new(AtomicUsize::new(0));
    let mut tools: Vec<ToolRegistration> = ["first", "second", "third"]
        .into_iter()
        .map(|name| registered_tool(name, false, Arc::clone(&executions)))
        .collect();
    for tool in &mut tools {
        tool.execute = Arc::new(CancellableTool);
    }
    Extension {
        name: "cancellable".to_string(),
        tools,
        hooks: vec![HookRegistration {
            task: "pi.tool".to_string(),
            handlers: HookHandlers::Tool(hooks as Arc<dyn rpi_durable::harness::ToolHooks>),
        }],
        ..Extension::default()
    }
}

#[tokio::test]
async fn after_tool_processes_interrupted_abandoned_and_aborted_results() {
    let storage = Arc::new(session_support::ControlledStorage::new());
    storage.keep_open_on_close();
    let hooks = Arc::new(InspectToolHooks::default());
    let opened = generation_support::open_with_storage(
        Arc::clone(&storage) as Arc<dyn Storage>,
        vec![cancellable_extension(Arc::clone(&hooks))],
        None,
        test_models(vec![tool_round(&[
            ("first", "c1"),
            ("second", "c2"),
            ("third", "c3"),
        ])]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let _running = submit_input(&conversation, "go").await;
    let mut tasks = Vec::new();
    for _ in 0..200 {
        tasks = opened
            .harness
            .inspect(&context())
            .await
            .expect("inspect")
            .tasks
            .into_iter()
            .filter(|task| task.record.kind == "pi.tool")
            .map(|task| task.record)
            .collect::<Vec<_>>();
        if tasks.len() == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(tasks.len(), 3);
    tasks.sort_by_key(|task| task.id);
    for (index, mut task) in tasks.into_iter().enumerate() {
        let checkpoint = if index == 1 {
            serde_json::json!({ "phase": "call" })
        } else {
            serde_json::json!({ "phase": "execute", "arguments": {}, "replay": "safe" })
        };
        task.state = TaskState::Running { checkpoint };
        task.abort_requested = true;
        task.abort_reason = (index != 2).then_some("restart".to_string());
        task.restarts = None;
        storage
            .commit_one(
                &[rpi_durable::types::StorageWrite::Task { value: task }],
                &context(),
            )
            .await
            .expect("abort fixture");
    }
    opened.harness.session().fail("simulated crash");
    let _ = opened.harness.session().closed().await;

    let reopened = generation_support::open_with_storage(
        Arc::clone(&storage) as Arc<dyn Storage>,
        vec![cancellable_extension(Arc::clone(&hooks))],
        None,
        test_models(Vec::new()),
    )
    .await;
    reopened.harness.resume().expect("resume aborts");
    let mut terminal = Vec::new();
    for _ in 0..200 {
        terminal = storage
            .scan_tasks(
                TaskQuery {
                    kind: Some("pi.tool".to_string()),
                    status: Some(TaskStatus::Terminal),
                    ..Default::default()
                },
                10,
                None,
                &context(),
            )
            .await
            .expect("terminal tasks")
            .items;
        if terminal.len() == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(terminal.len(), 3);
    let mut codes = hooks.codes.lock().expect("codes").clone();
    codes.sort();
    assert_eq!(codes, ["abandoned", "aborted", "interrupted"]);
    assert!(
        hooks
            .arguments
            .lock()
            .expect("arguments")
            .iter()
            .all(|arguments| arguments.is_empty())
    );
    reopened.harness.close(&context()).await.expect("close");
}
