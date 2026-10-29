//! Generation task integration tests (port of the generation cases of
//! `packages/durable/test/harness-inbox.test.ts` and `harness-tasks.test.ts`
//! @ `4a080252f`).
//!
//! Every case drives the real `pi.generation` built-in through a Harness over
//! memory storage with a scripted provider (see `generation_support`).

mod generation_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rpi_ai::types::{
    AssistantContent, AssistantMessage, Message, StopReason, Tool as AiTool, ToolResultContent,
};
use rpi_durable::chord::context::Context;
use rpi_durable::harness::compaction::create_compaction;
use rpi_durable::harness::types::{
    CompactionPolicyPatch, ConversationRetryPolicyPatch, DiagnosticSeverity, ToolExecuteError,
};
use rpi_durable::harness::{
    CompactionReason, ContinueInput, Extension, GenerationHooks, HarnessSettings, HookApi,
    HookHandlers, HookRegistration, LIVE_DOC, LiveState, RequestMessages, ToolDiagnostic,
    ToolExecute, ToolExecutionApi, ToolExecutionResult, ToolRegistration, ToolReplay,
};
use rpi_durable::ids::{ConversationId, EntryId, TaskId};
use rpi_durable::session::{DocOwner, EntryDraft};
use rpi_durable::types::{EntryRecord, InputSubmissionStatus, SubmissionRecord};

use generation_support::{
    Script, context, entries, error_message, open, open_with_sources, root_ready, submit_input,
    test_models, text_message, tool_use_message,
};

/// Assistant text of every `pi.assistant` entry, in order.
fn assistant_texts(entries: &[EntryRecord]) -> Vec<String> {
    let mut texts = Vec::new();
    for entry in entries {
        let Some(messages) = &entry.model else {
            continue;
        };
        for message in messages {
            if let Message::Assistant(assistant) = message {
                let text = assistant
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        AssistantContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                texts.push(text);
            }
        }
    }
    texts
}

/// The text of every `pi.tool-result` entry's message.
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
                            ToolResultContent::Text(text) => Some(text.text.clone()),
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

/// The task ID currently owning `pi.live.run`, if any.
async fn run_task_id(
    opened: &generation_support::Opened,
    conversation_id: ConversationId,
) -> Option<TaskId> {
    let live = opened
        .harness
        .session()
        .snapshot(
            &LIVE_DOC,
            DocOwner::Conversation(conversation_id),
            &context(),
        )
        .await
        .ok()??;
    let state: LiveState =
        serde_json::from_value(serde_json::Value::Object(live)).expect("live state");
    state.run.map(|run| run.task_id)
}

fn input_status(record: &SubmissionRecord) -> Option<&InputSubmissionStatus> {
    match record {
        SubmissionRecord::Input { status, .. } => Some(status),
        SubmissionRecord::Write { .. } => None,
    }
}

/// upstream: "answers with a committed assistant entry and ends the run done".
#[tokio::test]
async fn answer_appends_assistant_entry_and_ends_run_done() {
    let opened = open(
        Vec::new(),
        None,
        test_models(vec![Script::Message(text_message(
            "hello",
            StopReason::Stop,
        ))]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let settled = submission.wait(&context()).await.expect("wait");
    assert!(
        matches!(
            input_status(&settled),
            Some(InputSubmissionStatus::Done { .. })
        ),
        "expected Done, got {settled:?}"
    );
    assert_eq!(
        assistant_texts(&entries(&conversation).await),
        vec!["hello"]
    );
    assert!(run_task_id(&opened, conversation.id()).await.is_none());
}

/// durable/harness/types.ts:616-623 and harness.ts:187-198: the Harness
/// applies its identity over the Models defaults for every model request.
#[tokio::test]
async fn harness_model_requests_apply_identity_overrides() {
    let custom = rpi_ai::utils::identity::ClientAttribution {
        name: "custom-harness".to_owned(),
        url: "https://example.test".to_owned(),
        categories: None,
    };
    for (identity, expected_attribution) in [
        (None, Some(rpi_ai::utils::identity::rpi_attribution())),
        (
            Some(rpi_ai::utils::identity::ClientIdentity {
                attribution: Some(Some(custom.clone())),
                ..Default::default()
            }),
            Some(custom),
        ),
        (
            Some(rpi_ai::utils::identity::ClientIdentity {
                attribution: Some(None),
                ..Default::default()
            }),
            None,
        ),
    ] {
        let (models, identity_log) =
            generation_support::test_models_with_identity_log(vec![Script::Message(text_message(
                "hello",
                StopReason::Stop,
            ))]);
        let opened =
            generation_support::open_with_identity(Vec::new(), None, models, identity).await;
        let conversation = root_ready(&opened.harness).await;
        let submission = submit_input(&conversation, "hi").await;
        let settled = submission.wait(&context()).await.expect("wait");
        assert!(
            matches!(
                input_status(&settled),
                Some(InputSubmissionStatus::Done { .. })
            ),
            "expected Done, got {settled:?}"
        );
        let identities = identity_log.lock().expect("identity log").clone();
        assert_eq!(identities.len(), 1);
        let actual = identities[0]
            .as_ref()
            .and_then(|identity| identity.attribution.as_ref())
            .and_then(|attribution| attribution.as_ref());
        assert_eq!(actual, expected_attribution.as_ref());
    }
}

/// upstream: "retries a retryable assistant error, then succeeds".
#[tokio::test]
async fn retryable_error_retries_then_succeeds() {
    let settings = HarnessSettings {
        retry: Some(ConversationRetryPolicyPatch {
            base_delay_ms: Some(1),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opened = open(
        Vec::new(),
        Some(settings),
        test_models(vec![
            Script::Message(error_message("transient", "Please retry your request")),
            Script::Message(text_message("ok", StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let settled = submission.wait(&context()).await.expect("wait");
    assert!(
        matches!(
            input_status(&settled),
            Some(InputSubmissionStatus::Done { .. })
        ),
        "expected Done, got {settled:?}"
    );
    // The failed attempt's assistant entry is committed too.
    assert_eq!(
        assistant_texts(&entries(&conversation).await),
        vec!["", "ok"]
    );
}

/// upstream: "fails model_error on a context overflow without compaction".
#[tokio::test]
async fn overflow_without_compaction_fails_model_error() {
    let settings = HarnessSettings {
        compaction: Some(CompactionPolicyPatch {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opened = open(
        Vec::new(),
        Some(settings),
        test_models(vec![Script::Message(error_message(
            "overflow",
            "This model's maximum context length is 200000 tokens, but 208432 were requested",
        ))]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let settled = submission.wait(&context()).await.expect("wait");
    match input_status(&settled) {
        Some(InputSubmissionStatus::Unanswered { reason, .. }) => {
            assert_eq!(reason, "model_error");
        }
        other => panic!("expected Unanswered(model_error), got {other:?}"),
    }
}

/// upstream: "settles a deferred response as model_error (deferred is [DEFER])".
#[tokio::test]
async fn deferred_response_fails_model_error() {
    let mut deferred = text_message("", StopReason::Deferred);
    deferred.deferred = Some(Box::new(rpi_ai::types::DeferredHandle {
        provider: "test".to_string(),
        model_id: "test-model".to_string(),
        api: "openai".to_string(),
        id: "d1".to_string(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    }));
    let opened = open(
        Vec::new(),
        None,
        test_models(vec![Script::Message(deferred)]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let settled = submission.wait(&context()).await.expect("wait");
    match input_status(&settled) {
        Some(InputSubmissionStatus::Unanswered { reason, .. }) => {
            assert_eq!(reason, "model_error");
        }
        other => panic!("expected Unanswered(model_error), got {other:?}"),
    }
}

/// upstream: "fails no_model when the agent has no model".
#[tokio::test]
async fn no_model_fails_no_model() {
    let opened = open(
        Vec::new(),
        None,
        test_models(vec![Script::Message(text_message(
            "unused",
            StopReason::Stop,
        ))]),
    )
    .await;
    let conversation = generation_support::root_only(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let settled = submission.wait(&context()).await.expect("wait");
    match input_status(&settled) {
        Some(InputSubmissionStatus::Unanswered { reason, .. }) => assert_eq!(reason, "no_model"),
        other => panic!("expected Unanswered(no_model), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tool round
// ---------------------------------------------------------------------------

struct EchoTool;

#[async_trait]
impl ToolExecute for EchoTool {
    async fn execute(
        &self,
        _args: rpi_durable::chord::json::JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        api.output(b"hello output", None).expect("output");
        api.diagnostic(ToolDiagnostic {
            severity: DiagnosticSeverity::Warn,
            message: "note".to_string(),
            code: Some("note".to_string()),
        });
        Ok(ToolExecutionResult::default())
    }
}

fn echo_extension() -> Extension {
    Extension {
        name: "test".to_string(),
        tools: vec![ToolRegistration {
            tool: AiTool {
                name: "echo".to_string(),
                description: "echo".to_string(),
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
                constrained_sampling: None,
            },
            replay: ToolReplay::Unsafe,
            execution_mode: None,
            prepare_arguments: None,
            output_limits: None,
            structured_output_schema: None,
            callers: None,
            deferred: false,
            execute: Arc::new(EchoTool),
        }],
        ..Extension::default()
    }
}

/// upstream: "dispatches a tool task, appends its result, and issues a follow-up".
#[tokio::test]
async fn tool_round_dispatches_result_and_follow_up() {
    let opened = open(
        vec![echo_extension()],
        None,
        test_models(vec![
            Script::Message(tool_use_message("echo", "c1", serde_json::json!({}))),
            Script::Message(text_message("done", StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let settled = submission.wait(&context()).await.expect("wait");
    assert!(
        matches!(
            input_status(&settled),
            Some(InputSubmissionStatus::Done { .. })
        ),
        "expected Done, got {settled:?}"
    );
    let entries = entries(&conversation).await;
    assert_eq!(assistant_texts(&entries), vec!["", "done"]);
    let results = tool_result_texts(&entries);
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("hello output"), "got {}", results[0]);
    assert!(results[0].contains("[warn] note"), "got {}", results[0]);
}

// ---------------------------------------------------------------------------
// Abort
// ---------------------------------------------------------------------------

/// upstream: "the first onYield continuation appends a user message and hands over".
#[tokio::test]
async fn on_yield_continuation_hands_over() {
    struct ContinueOnce(AtomicBool);

    #[async_trait]
    impl GenerationHooks for ContinueOnce {
        async fn before_request(
            &self,
            _request: RequestMessages,
            _api: &dyn HookApi,
            _context: &Context,
        ) -> Option<RequestMessages> {
            None
        }

        async fn after_response(
            &self,
            _message: &AssistantMessage,
            _api: &dyn HookApi,
            _context: &Context,
        ) {
        }

        async fn on_yield(
            &self,
            _answer: &AssistantMessage,
            _api: &dyn HookApi,
            _context: &Context,
        ) -> Option<ContinueInput> {
            if self.0.swap(true, Ordering::SeqCst) {
                return None;
            }
            Some(ContinueInput {
                continue_input: rpi_ai::types::UserContent::Text("again".to_string()),
            })
        }

        async fn after_tools(
            &self,
            _assistant: EntryId,
            _results: &[EntryId],
            _api: &dyn HookApi,
            _context: &Context,
        ) {
        }
    }

    let hooks = Extension {
        name: "continue".to_string(),
        hooks: vec![HookRegistration {
            task: "pi.generation".to_string(),
            handlers: HookHandlers::Generation(Arc::new(ContinueOnce(AtomicBool::new(false)))),
        }],
        ..Extension::default()
    };
    let opened = open(
        vec![hooks],
        None,
        test_models(vec![
            Script::Message(text_message("first", StopReason::Stop)),
            Script::Message(text_message("second", StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let settled = submission.wait(&context()).await.expect("wait");
    assert!(
        matches!(
            input_status(&settled),
            Some(InputSubmissionStatus::Done { .. })
        ),
        "expected Done, got {settled:?}"
    );
    let entries = entries(&conversation).await;
    assert_eq!(assistant_texts(&entries), vec!["first", "second"]);
    let users: Vec<String> = entries
        .iter()
        .flat_map(|entry| entry.model.iter().flatten())
        .filter_map(|message| match message {
            Message::User(user) => Some(serde_json::to_string(&user.content).unwrap_or_default()),
            _ => None,
        })
        .collect();
    assert!(
        users.iter().any(|user| user.contains("again")),
        "got {users:?}"
    );
}

/// upstream: "an aborted run appends an aborted assistant entry and settles aborted".
#[tokio::test]
async fn abort_mid_stream_appends_aborted_result() {
    let opened = open(
        Vec::new(),
        None,
        test_models(vec![Script::Pending("thinking".to_string())]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    let mut task_id = None;
    for _ in 0..200 {
        task_id = run_task_id(&opened, conversation.id()).await;
        if task_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let task_id = task_id.expect("run started");
    // Let the throttled partial flush land before aborting.
    tokio::time::sleep(Duration::from_millis(200)).await;
    opened
        .harness
        .abort_task(task_id, &context())
        .await
        .expect("abort");
    let settled = submission.wait(&context()).await.expect("wait");
    match input_status(&settled) {
        Some(InputSubmissionStatus::Unanswered { reason, .. }) => assert_eq!(reason, "aborted"),
        other => panic!("expected Unanswered(aborted), got {other:?}"),
    }
    let aborted = entries(&conversation).await.into_iter().any(|entry| {
        entry.model.iter().flatten().any(|message| {
            matches!(message, Message::Assistant(assistant) if assistant.stop_reason == StopReason::Aborted)
        })
    });
    assert!(aborted, "expected an aborted assistant entry");
}

// ---------------------------------------------------------------------------
// Compaction basics
// ---------------------------------------------------------------------------

/// upstream: "a manual compaction places a summary entry with the first kept head".
#[tokio::test]
async fn manual_compaction_places_summary() {
    let settings = HarnessSettings {
        compaction: Some(CompactionPolicyPatch {
            keep_recent_tokens: Some(1),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opened = open(
        Vec::new(),
        Some(settings),
        test_models(vec![Script::Message(text_message(
            "the summary",
            StopReason::Stop,
        ))]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let conversation_id = conversation.id();
    for index in 0..4 {
        let change = rpi_durable::session::commit_fn(move |tx| {
            Box::pin(async move {
                let mut draft = EntryDraft::new(rpi_durable::entries::USER_ENTRY);
                draft.model = Some(vec![Message::User(rpi_ai::types::UserMessage {
                    role: rpi_ai::types::UserRole::User,
                    content: rpi_ai::types::UserContent::Text(format!("message number {index}")),
                    timestamp: index as i64,
                })]);
                tx.append_entry(conversation_id, draft).await?;
                Ok(())
            })
        });
        opened
            .harness
            .session()
            .commit(change, &context())
            .await
            .expect("append");
    }
    let create = rpi_durable::session::commit_fn(move |tx| {
        Box::pin(async move {
            create_compaction(tx, conversation_id, CompactionReason::Manual, None, None).await
        })
    });
    let task_id = opened
        .harness
        .session()
        .commit(create, &context())
        .await
        .expect("create compaction");
    let settled = opened
        .harness
        .wait_for_task(task_id, &context())
        .await
        .expect("wait compaction");
    assert!(matches!(
        settled.record.state,
        rpi_durable::types::TaskState::Terminal { .. }
    ));
    let entries = entries(&conversation).await;
    let compaction = entries
        .iter()
        .find(|entry| entry.kind == rpi_durable::entries::COMPACTION_ENTRY)
        .expect("compaction entry");
    assert!(compaction.head.is_some());
    assert_eq!(
        compaction.data.as_ref().and_then(|data| data.get("reason")),
        Some(&serde_json::json!("manual"))
    );
    let text = compaction
        .model
        .iter()
        .flatten()
        .find_map(|message| match message {
            Message::User(user) => Some(serde_json::to_string(&user.content).unwrap_or_default()),
            _ => None,
        })
        .unwrap_or_default();
    assert!(text.contains("the summary"), "got {text}");
}

/// V18-06 FR-F: a dynamic model source is read at each use, not cached at open.
#[tokio::test]
async fn dynamic_models_source_is_read_for_each_request() {
    use rpi_durable::harness::ModelsSource;
    use rpi_durable::storage::MemoryStorage;

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let base = test_models(vec![Script::Message(text_message("ok", StopReason::Stop))]);
    let counter = Arc::clone(&calls);
    let source = ModelsSource::Dynamic(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        base.clone()
    }));
    let opened = open_with_sources(
        Arc::new(MemoryStorage::new()),
        Vec::new(),
        None,
        source,
        None,
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

/// V18-06 FR-F: a dynamic settings source is read at each scheduling use.
#[tokio::test]
async fn dynamic_settings_source_is_read_for_each_use() {
    use rpi_durable::harness::{HarnessSettings, ModelsSource, SettingsSource};
    use rpi_durable::storage::MemoryStorage;

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let source = SettingsSource::Dynamic(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Some(HarnessSettings::default())
    }));
    let opened = open_with_sources(
        Arc::new(MemoryStorage::new()),
        Vec::new(),
        Some(source),
        ModelsSource::Fixed(test_models(vec![Script::Message(text_message(
            "ok",
            StopReason::Stop,
        ))])),
        None,
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let submission = submit_input(&conversation, "hi").await;
    submission.wait(&context()).await.expect("wait");
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

// ---------------------------------------------------------------------------
// afterFailedRun (V18-06 FR-H R4)
// ---------------------------------------------------------------------------

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
        api.output(b"released", None).expect("output");
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

async fn queue_follow_up(
    conversation: &Arc<dyn rpi_durable::harness::Conversation>,
    content: &str,
) -> Arc<dyn rpi_durable::harness::Submission> {
    use rpi_durable::harness::{InputSubmissionDraft, SubmissionDraft, WhenBusy};
    conversation
        .submit(
            SubmissionDraft::Input(InputSubmissionDraft {
                request_id: None,
                content: rpi_ai::types::UserContent::Text(content.to_string()),
                when_busy: Some(WhenBusy::FollowUp),
                entry: None,
            }),
            &context(),
        )
        .await
        .expect("queue follow-up")
}

/// `withdraw` settles the queued input `unanswered` with reason `run_failed`.
#[tokio::test]
async fn after_failed_run_withdraw_settles_queued_inputs() {
    use rpi_durable::harness::{AfterFailedRun, HarnessSettings};
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let settings = HarnessSettings {
        after_failed_run: Some(AfterFailedRun::Withdraw),
        retry: Some(ConversationRetryPolicyPatch {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opened = open(
        vec![blocking_extension(
            Arc::clone(&started),
            Arc::clone(&release),
        )],
        Some(settings),
        test_models(vec![
            Script::Message(tool_use_message("block", "c1", serde_json::json!({}))),
            Script::Message(error_message("fatal", "bad request")),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let running = submit_input(&conversation, "hi").await;
    started.notified().await;
    let queued = queue_follow_up(&conversation, "later").await;
    release.notify_one();
    let _first = running.wait(&context()).await.expect("first run");
    let record = queued.status(&context()).await.expect("queued status");
    assert_eq!(record.status(), "unanswered");
    match record {
        SubmissionRecord::Input {
            status: InputSubmissionStatus::Unanswered { reason, .. },
            ..
        } => assert_eq!(reason, "run_failed"),
        other => panic!("unexpected record {other:?}"),
    }
}

/// `continue` starts the next run with the queued input.
#[tokio::test]
async fn after_failed_run_continue_starts_the_next_run() {
    use rpi_durable::harness::{AfterFailedRun, HarnessSettings};
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let settings = HarnessSettings {
        after_failed_run: Some(AfterFailedRun::Continue),
        retry: Some(ConversationRetryPolicyPatch {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opened = open(
        vec![blocking_extension(
            Arc::clone(&started),
            Arc::clone(&release),
        )],
        Some(settings),
        test_models(vec![
            Script::Message(tool_use_message("block", "c1", serde_json::json!({}))),
            Script::Message(error_message("fatal", "bad request")),
            Script::Message(text_message("recovered", StopReason::Stop)),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let running = submit_input(&conversation, "hi").await;
    started.notified().await;
    let queued = queue_follow_up(&conversation, "later").await;
    release.notify_one();
    running.wait(&context()).await.expect("first run");
    let record = queued.wait(&context()).await.expect("second run");
    assert_eq!(record.status(), "done");
}

/// `keep` leaves queued inputs queued after the failed run.
#[tokio::test]
async fn after_failed_run_keep_leaves_the_queue() {
    use rpi_durable::harness::{AfterFailedRun, HarnessSettings};
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
    let opened = open(
        vec![blocking_extension(
            Arc::clone(&started),
            Arc::clone(&release),
        )],
        Some(settings),
        test_models(vec![
            Script::Message(tool_use_message("block", "c1", serde_json::json!({}))),
            Script::Message(error_message("fatal", "bad request")),
        ]),
    )
    .await;
    let conversation = root_ready(&opened.harness).await;
    let running = submit_input(&conversation, "hi").await;
    started.notified().await;
    let queued = queue_follow_up(&conversation, "later").await;
    release.notify_one();
    running.wait(&context()).await.expect("first run");
    assert_eq!(
        queued.status(&context()).await.expect("status").status(),
        "queued"
    );
}
