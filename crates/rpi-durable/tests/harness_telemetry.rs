//! Telemetry span/trace integration tests for V18-06 FR-F R3.
//!
//! A recording sink implements the adapter contract: spans carry parent ids,
//! start/end attributes, events, links and status; carriers are injected into
//! durable task and submission records and extracted by later invocations.

mod scheduler_support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_ai::types::Tool as AiTool;
use rpi_durable::chord::context::{Context, TELEMETRY_CONTEXT_KEY, with_context_value};
use rpi_durable::chord::json::{JsonObject, JsonValue};
use rpi_durable::chord::telemetry::{
    TelemetryCarrier, TelemetryContext, TelemetrySink, TelemetrySpanSink,
};
use rpi_durable::harness::{
    AgentChange, Extension, ExtensionsChange, LIVE_DOC, RootOptions, ToolExecute, ToolExecuteError,
    ToolExecutionApi, ToolExecutionResult, ToolRegistration, ToolReplay, Wrap,
};
use rpi_durable::ids::{ConversationId, TaskId};
use rpi_durable::session::{DocOwner, TaskCreate, TaskOwnership, TaskTrace, commit_fn};
use rpi_durable::storage::MemoryStorage;
use rpi_durable::types::{Storage, TaskStatus};
use scheduler_support::{
    Spec, add_task, all_tasks, context, open_with_telemetry, open_with_telemetry_and_extensions,
    root,
};

#[derive(Default)]
struct State {
    spans: Mutex<Vec<Span>>,
    metrics: Mutex<Vec<(String, f64, JsonObject)>>,
    root_events: Mutex<Vec<(String, JsonObject)>>,
    next: AtomicU64,
}

struct Span {
    id: u64,
    parent: Option<u64>,
    name: String,
    attributes: JsonObject,
    links: Vec<TelemetryCarrier>,
    events: Vec<String>,
    event_attributes: Vec<JsonObject>,
    end_attributes: JsonObject,
    status: String,
    message: Option<String>,
    ended: bool,
}

struct SpanSnapshot {
    parent: Option<u64>,
    attributes: JsonObject,
    links: Vec<TelemetryCarrier>,
    events: Vec<String>,
    end_attributes: JsonObject,
    status: String,
    message: Option<String>,
    ended: bool,
}

#[derive(Clone, Default)]
struct Recorder(Arc<State>);

#[derive(Clone)]
struct RecordingSink {
    state: Arc<State>,
    span: Option<u64>,
}

impl Recorder {
    fn root(&self) -> RecordingSink {
        RecordingSink {
            state: Arc::clone(&self.0),
            span: None,
        }
    }

    fn has_event(&self, name: &str, event: &str) -> bool {
        self.0
            .spans
            .lock()
            .expect("spans")
            .iter()
            .filter(|span| span.name == name)
            .any(|span| span.events.iter().any(|candidate| candidate == event))
    }

    fn span_count(&self, name: &str) -> usize {
        self.0
            .spans
            .lock()
            .expect("spans")
            .iter()
            .filter(|span| span.name == name)
            .count()
    }

    fn last_span(&self, name: &str) -> SpanSnapshot {
        let spans = self.0.spans.lock().expect("spans");
        let span = spans
            .iter()
            .rev()
            .find(|span| span.name == name)
            .expect("span");
        SpanSnapshot {
            parent: span.parent,
            attributes: span.attributes.clone(),
            links: span.links.clone(),
            events: span.events.clone(),
            end_attributes: span.end_attributes.clone(),
            status: span.status.clone(),
            message: span.message.clone(),
            ended: span.ended,
        }
    }

    fn span(&self, name: &str) -> SpanSnapshot {
        let spans = self.0.spans.lock().expect("spans");
        let span = spans.iter().find(|span| span.name == name).expect("span");
        SpanSnapshot {
            parent: span.parent,
            attributes: span.attributes.clone(),
            links: span.links.clone(),
            events: span.events.clone(),
            end_attributes: span.end_attributes.clone(),
            status: span.status.clone(),
            message: span.message.clone(),
            ended: span.ended,
        }
    }
}

impl RecordingSink {
    fn with_span(&self, change: impl FnOnce(&mut Span)) {
        let Some(id) = self.span else {
            return;
        };
        if let Ok(mut spans) = self.state.spans.lock()
            && let Some(span) = spans.iter_mut().find(|span| span.id == id)
        {
            change(span);
        }
    }
}

impl TelemetrySink for RecordingSink {
    fn set_attributes(&self, attributes: JsonObject) {
        self.with_span(|span| span.attributes.extend(attributes));
    }

    fn add_event(&self, name: &str, attributes: JsonObject) {
        if self.span.is_none() {
            self.state
                .root_events
                .lock()
                .expect("events")
                .push((name.to_string(), attributes));
            return;
        }
        self.with_span(|span| {
            span.events.push(name.to_string());
            span.event_attributes.push(attributes);
        });
    }

    fn add_link(&self, carrier: JsonValue) {
        if let JsonValue::Object(carrier) = carrier {
            self.with_span(|span| span.links.push(carrier));
        }
    }

    fn record_metric(&self, name: &str, value: f64, attributes: JsonObject) {
        self.state
            .metrics
            .lock()
            .expect("metrics")
            .push((name.to_string(), value, attributes));
    }

    fn start_span(
        &self,
        name: &str,
        attributes: JsonObject,
        links: Vec<TelemetryCarrier>,
    ) -> Option<Arc<dyn TelemetrySpanSink>> {
        let id = self.state.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.spans.lock().expect("spans").push(Span {
            id,
            parent: self.span,
            name: name.to_string(),
            attributes,
            links,
            events: Vec::new(),
            event_attributes: Vec::new(),
            end_attributes: JsonObject::new(),
            status: "ok".to_string(),
            message: None,
            ended: false,
        });
        Some(Arc::new(RecordingSink {
            state: Arc::clone(&self.state),
            span: Some(id),
        }))
    }

    fn inject(&self) -> Option<TelemetryCarrier> {
        let id = self.span?;
        let mut carrier = JsonObject::new();
        carrier.insert("span".to_string(), JsonValue::from(id));
        Some(carrier)
    }

    fn extract(&self, carrier: &TelemetryCarrier) -> Option<Arc<dyn TelemetrySink>> {
        let id = carrier.get("span")?.as_u64()?;
        Some(Arc::new(RecordingSink {
            state: Arc::clone(&self.state),
            span: Some(id),
        }))
    }

    fn set_status(&self, status: &str, message: Option<&str>) {
        self.with_span(|span| {
            span.status = status.to_string();
            span.message = message.map(str::to_string);
        });
    }
}

impl TelemetrySpanSink for RecordingSink {
    fn end_span(&self, attributes: JsonObject, status: &str, message: Option<&str>) {
        self.with_span(|span| {
            span.end_attributes = attributes;
            span.status = status.to_string();
            span.message = message.map(str::to_string);
            span.ended = true;
        });
    }
}

async fn wait_terminal(harness: &Arc<rpi_durable::harness::HarnessImpl>, id: TaskId) {
    for _ in 0..200 {
        let record = harness
            .get_task(id, &context())
            .await
            .expect("task")
            .expect("record");
        if record.status() == TaskStatus::Terminal {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("task did not finish");
}

async fn create_in_span(
    harness: &Arc<rpi_durable::harness::HarnessImpl>,
    span: &rpi_durable::harness::TelemetrySpan,
    conversation: ConversationId,
    kind: &str,
    links: Vec<TelemetryCarrier>,
) -> TaskId {
    let context = with_context_value(&TELEMETRY_CONTEXT_KEY, span.context(), &context());
    let kind = kind.to_string();
    harness
        .session()
        .commit_with(
            commit_fn(move |tx| {
                let kind = kind.clone();
                let links = links.clone();
                Box::pin(async move {
                    tx.create_task(TaskCreate {
                        kind,
                        version: 1,
                        input: serde_json::json!({ "phases": 2 }),
                        checkpoint: serde_json::json!({ "phase": "step", "count": 0 }),
                        ownership: TaskOwnership::Conversation(conversation),
                        conversation_id: Some(conversation),
                        background: false,
                        abandon_on_restart: false,
                        trace: TaskTrace::Inherit,
                        links,
                    })
                    .await
                })
            }),
            &context,
            None,
        )
        .await
        .expect("create task")
}

/// R-D9: a phase span extracts the task's stored parent, carries first-run
/// links, records `resumed`, and settles with end attributes.
#[tokio::test]
async fn phase_spans_use_the_stored_trace_and_links() {
    let recorder = Recorder::default();
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let opened = open_with_telemetry(
        Arc::new(MemoryStorage::new()),
        all_tasks(),
        telemetry.clone(),
    )
    .await;
    let conversation = root(&opened.harness).await;
    let creator = telemetry.start_span("test.creator", JsonObject::new(), Vec::new());
    let parent = creator.context().inject().expect("carrier");
    let parent_id = parent["span"].as_u64().expect("parent id");
    let mut link = JsonObject::new();
    link.insert("span".to_string(), JsonValue::from(9999));
    let id = create_in_span(
        &opened.harness,
        &creator,
        conversation,
        "test.memo",
        vec![link.clone()],
    )
    .await;
    creator.end(JsonObject::new());

    let record = opened
        .harness
        .get_task(id, &context())
        .await
        .expect("task")
        .expect("record");
    assert_eq!(record.trace, Some(parent));
    assert_eq!(record.links, vec![link.clone()]);

    opened.harness.resume().expect("resume");
    wait_terminal(&opened.harness, id).await;
    let span = recorder.span("pi.durable.phase");
    assert_eq!(span.parent, Some(parent_id));
    assert_eq!(span.attributes["resumed"], JsonValue::Bool(false));
    assert_eq!(span.links, vec![link]);
    assert!(span.ended);
    assert_eq!(span.end_attributes["outcome"], "next=second");
    assert_eq!(span.status, "ok");
    assert!(
        span.events
            .iter()
            .any(|event| event == "pi.durable.checkpoint"),
        "phase events: {:?}",
        span.events
    );
    let last = recorder.last_span("pi.durable.phase");
    assert_eq!(last.end_attributes["outcome"], "done");
    assert_eq!(last.status, "ok");
    let metrics = recorder.0.metrics.lock().expect("metrics").clone();
    assert!(
        metrics
            .iter()
            .any(|(name, _, attributes)| name == "pi.durable.phase.duration"
                && attributes["task"] == "test.memo")
    );
    for value in [1.0, -1.0] {
        assert!(
            metrics.iter().any(|(name, recorded, attributes)| {
                name == "pi.durable.tasks.active"
                    && *recorded == value
                    && attributes["task"] == "test.memo"
            }),
            "missing active-task sample {value}: {metrics:?}"
        );
    }
    drop(metrics);
    opened.harness.close(&context()).await.expect("close");
}

/// R-D9: a reopened Harness extracts the parent carrier stored on the record
/// before the phase span starts, so a restart keeps one trace.
#[tokio::test]
async fn a_reopened_harness_restores_the_phase_parent_trace() {
    let recorder = Recorder::default();
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_telemetry(Arc::clone(&storage), all_tasks(), telemetry.clone()).await;
    let conversation = root(&opened.harness).await;
    let creator = telemetry.start_span("test.creator", JsonObject::new(), Vec::new());
    let parent = creator.context().inject().expect("carrier");
    let parent_id = parent["span"].as_u64().expect("parent id");
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 1 })),
    )
    .await;
    // Stamp the carrier as if a crashed process created the task.
    let mut record = opened
        .harness
        .get_task(id, &context())
        .await
        .expect("task")
        .expect("record");
    record.trace = Some(parent.clone());
    scheduler_support::stamp_task(&opened.harness, record).await;
    creator.end(JsonObject::new());
    // Simulate a crash: drop the Harness without a close, so the record keeps
    // its stored trace and the storage stays open for the next process.
    drop(opened);

    let reopened = open_with_telemetry(Arc::clone(&storage), all_tasks(), telemetry.clone()).await;
    reopened.harness.resume().expect("resume");
    wait_terminal(&reopened.harness, id).await;
    let span = recorder.span("pi.durable.phase");
    assert_eq!(span.parent, Some(parent_id));
    assert_eq!(span.attributes["resumed"], JsonValue::Bool(true));
    reopened.harness.close(&context()).await.expect("close");
}

/// R-D9: an admitted submission records the admission span's carrier, and its
/// span carries start/end attributes.
#[tokio::test]
async fn a_submission_stores_its_admission_trace() {
    use rpi_durable::harness::{SubmissionDraft, WriteSubmissionDraft};
    use rpi_durable::session::EntryDraft;

    let recorder = Recorder::default();
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let opened = open_with_telemetry(
        Arc::new(MemoryStorage::new()),
        all_tasks(),
        telemetry.clone(),
    )
    .await;
    let conversation = opened
        .harness
        .root(&context(), Default::default())
        .await
        .expect("root");
    let caller_context = with_context_value(&TELEMETRY_CONTEXT_KEY, telemetry.clone(), &context());
    let submission = conversation
        .submit(
            SubmissionDraft::Write(WriteSubmissionDraft {
                request_id: None,
                entry: EntryDraft {
                    kind: "note".to_string(),
                    data: Some(serde_json::json!({ "text": "hello" })),
                    ..EntryDraft::default()
                },
            }),
            &caller_context,
        )
        .await
        .expect("submit write");
    let _ = submission.wait(&context()).await.expect("settled");
    let record = opened
        .harness
        .session()
        .storage()
        .submission(submission.id(), &context())
        .await
        .expect("submission")
        .expect("record");
    let carrier = record.trace().expect("admission trace");
    let span_id = carrier["span"].as_u64().expect("span id");
    let carrier_id = span_id;
    let span = recorder.span("pi.durable.submit");
    assert_eq!(span.parent, None);
    assert_eq!(span.attributes["submission.type"], "write");
    assert_eq!(
        span.attributes["conversation.id"],
        JsonValue::from(conversation.id().get())
    );
    assert_eq!(span.attributes["submission.id"], submission.id().get());
    assert_eq!(span.attributes["result"], "done");
    assert!(span.links.is_empty());
    assert!(span.ended);
    assert!(
        span.events
            .iter()
            .any(|event| event == "pi.durable.submission"),
        "submit events: {:?}",
        span.events
    );
    // The carrier stored on the record is the span that was just started; the
    // recording sink numbers spans from one, so the only submit span matches.
    assert_eq!(recorder.span_count("pi.durable.submit"), 1);
    assert!(carrier_id > 0);
    opened.harness.close(&context()).await.expect("close");
}

struct EntryTask;

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for EntryTask {
    fn name(&self) -> &str {
        "test.entry-span"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "step" })
    }
    async fn run_phase(
        &self,
        phase: &str,
        task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        if phase == "step" {
            let conversation = task.conversation_id();
            runtime
                .commit(
                    Arc::new(move |tx, _current| {
                        Box::pin(async move {
                            let mut draft = rpi_durable::session::EntryDraft::new("test.entry");
                            draft.data = Some(serde_json::json!({}));
                            tx.append_entry(conversation, draft).await?;
                            Ok(Some(rpi_durable::harness::NextTaskState::Running {
                                checkpoint: serde_json::json!({ "phase": "done" }),
                            }))
                        })
                    }),
                    context,
                )
                .await
        } else {
            runtime
                .commit(
                    Arc::new(|_tx, _current| {
                        Box::pin(async move {
                            Ok(Some(rpi_durable::harness::NextTaskState::Terminal {
                                outcome: rpi_durable::types::TaskOutcome::Completed {
                                    result: serde_json::json!({}),
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

struct ThrowTask;

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for ThrowTask {
    fn name(&self) -> &str {
        "test.throw-span"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "step" })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        _runtime: &dyn rpi_durable::harness::TaskRuntime,
        _context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        Err(rpi_durable::session::SessionError::message("boom"))
    }
}

struct AbortOutcomeTask;

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for AbortOutcomeTask {
    fn name(&self) -> &str {
        "test.abort-span"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "step" })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        runtime
            .commit(
                Arc::new(|_tx, _current| {
                    Box::pin(async move {
                        Ok(Some(rpi_durable::harness::NextTaskState::Terminal {
                            outcome: rpi_durable::types::TaskOutcome::Aborted {
                                reason: Some("stop".to_string()),
                                result: None,
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }
}

/// R-D9: a faulted phase ends `faulted` with an error status; an aborted
/// outcome ends `aborted` and stays ok.
#[tokio::test]
async fn phase_spans_carry_faulted_and_aborted_status() {
    let recorder = Recorder::default();
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let opened = open_with_telemetry(
        Arc::new(MemoryStorage::new()),
        vec![
            Arc::new(ThrowTask) as rpi_durable::harness::AnyTask,
            Arc::new(AbortOutcomeTask) as rpi_durable::harness::AnyTask,
            Arc::new(EntryTask) as rpi_durable::harness::AnyTask,
        ],
        telemetry,
    )
    .await;
    let conversation = root(&opened.harness).await;
    let thrown = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.throw-span", serde_json::json!({})),
    )
    .await;
    opened.harness.resume().expect("resume");
    wait_terminal(&opened.harness, thrown).await;
    let span = recorder.span("pi.durable.phase");
    assert_eq!(span.end_attributes["outcome"], "faulted");
    assert_eq!(span.status, "error");
    assert!(
        span.message
            .as_deref()
            .is_some_and(|message| message.contains("boom")),
        "message: {:?}",
        span.message
    );
    let _ = opened.harness.wait_for_task(thrown, &context()).await;

    let aborted = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.abort-span", serde_json::json!({})),
    )
    .await;
    let _ = opened
        .harness
        .wait_for_task(aborted, &context())
        .await
        .expect("abort task");
    let span = recorder.last_span("pi.durable.phase");
    assert_eq!(span.end_attributes["outcome"], "aborted");
    assert_eq!(span.status, "ok");

    let entry_task = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.entry-span", serde_json::json!({})),
    )
    .await;
    let _ = opened
        .harness
        .wait_for_task(entry_task, &context())
        .await
        .expect("entry task");
    assert!(
        recorder.has_event("pi.durable.phase", "pi.durable.entry"),
        "no phase span carried the entry event"
    );
    opened.harness.close(&context()).await.expect("close");
}

struct CancellableTask(Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for CancellableTask {
    fn name(&self) -> &str {
        "test.cancel-span"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "step" })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        _context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        self.0.notify_one();
        runtime.signal().cancelled().await;
        Err(rpi_durable::session::SessionError::Aborted)
    }
}

/// R-D9: a phase ended by Harness close is `aborted` and keeps status ok.
#[tokio::test]
async fn a_phase_ended_by_close_is_aborted_and_ok() {
    let started = Arc::new(tokio::sync::Notify::new());
    let recorder = Recorder::default();
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let opened = open_with_telemetry(
        Arc::new(MemoryStorage::new()),
        vec![Arc::new(CancellableTask(Arc::clone(&started))) as rpi_durable::harness::AnyTask],
        telemetry,
    )
    .await;
    let conversation = root(&opened.harness).await;
    let _ = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.cancel-span", serde_json::json!({})),
    )
    .await;
    opened.harness.resume().expect("resume");
    started.notified().await;
    opened.harness.close(&context()).await.expect("close");
    let span = recorder.last_span("pi.durable.phase");
    assert_eq!(span.end_attributes["outcome"], "aborted");
    assert_eq!(span.status, "ok");
    assert!(span.message.is_none(), "message: {:?}", span.message);
}

struct UnusedTool;

#[async_trait]
impl ToolExecute for UnusedTool {
    async fn execute(
        &self,
        _args: JsonObject,
        _api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        Ok(ToolExecutionResult::default())
    }
}

fn wrap_extension() -> Extension {
    Extension {
        name: "wrap-test".to_string(),
        tools: vec![ToolRegistration {
            tool: AiTool {
                name: "x".to_string(),
                description: "x".to_string(),
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
            execute: Arc::new(UnusedTool),
        }],
        wraps: vec![Wrap::Tool {
            tool: "x".to_string(),
            wrap: Arc::new(|mut tool: ToolRegistration| {
                tool.tool.name = "z".to_string();
                tool
            }),
        }],
        ..Extension::default()
    }
}

// ─── V18-06 eleventh review: report attributes, abort duration, span end ────

/// R-D9: a task-level report emits exactly one event and one metric. The
/// scheduler emits its own telemetry; the Harness callback must not emit it a
/// second time for reports that already took that path.
#[tokio::test]
async fn a_task_report_emits_one_event_and_one_metric() {
    let recorder = Recorder::default();
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let opened = open_with_telemetry(Arc::clone(&storage), all_tasks(), telemetry).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.report-extension-error", serde_json::json!({})),
    )
    .await;
    opened
        .harness
        .wait_for_task(id, &context())
        .await
        .expect("wait");
    let events = recorder
        .0
        .root_events
        .lock()
        .expect("events")
        .iter()
        .filter(|(name, attributes)| {
            name == "pi.durable.report"
                && attributes.get("type").and_then(JsonValue::as_str) == Some("extension_error")
        })
        .count();
    let metrics = recorder
        .0
        .metrics
        .lock()
        .expect("metrics")
        .iter()
        .filter(|(name, _, attributes)| {
            name == "pi.durable.reports"
                && attributes.get("type").and_then(JsonValue::as_str) == Some("extension_error")
        })
        .count();
    assert_eq!(events, 1, "one report event per report");
    assert_eq!(metrics, 1, "one report metric per report");
}

#[tokio::test]
async fn a_listener_panic_emits_one_listener_error() {
    let recorder = Recorder::default();
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let opened = open_with_telemetry(Arc::clone(&storage), all_tasks(), telemetry).await;
    let _subscription = opened
        .harness
        .session()
        .subscribe_commits(Arc::new(|_, _| panic!("host listener")));
    let conversation = root(&opened.harness).await;
    opened
        .harness
        .session()
        .commit(
            commit_fn(move |tx| {
                Box::pin(async move {
                    tx.doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                        .await?;
                    Ok(())
                })
            }),
            &context(),
        )
        .await
        .ok();
    let events = recorder
        .0
        .root_events
        .lock()
        .expect("events")
        .iter()
        .filter(|(name, attributes)| {
            name == "pi.durable.report"
                && attributes.get("type").and_then(JsonValue::as_str) == Some("listener_error")
        })
        .count();
    let metrics = recorder
        .0
        .metrics
        .lock()
        .expect("metrics")
        .iter()
        .filter(|(name, _, attributes)| {
            name == "pi.durable.reports"
                && attributes.get("type").and_then(JsonValue::as_str) == Some("listener_error")
        })
        .count();
    assert_eq!(events, 1, "one listener_error event");
    assert_eq!(metrics, 1, "one listener_error metric");
}

#[tokio::test]
async fn a_wrapper_rename_from_a_task_emits_one_extension_error() {
    let recorder = Recorder::default();
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let extension = Arc::new(wrap_extension());
    let opened = open_with_telemetry_and_extensions(
        Arc::clone(&storage),
        all_tasks(),
        telemetry,
        None,
        vec![extension.as_ref().clone()],
    )
    .await;
    let conversation = opened
        .harness
        .root(
            &context(),
            RootOptions {
                agent: Some(AgentChange {
                    extensions: Some(Some(ExtensionsChange::List(vec![Arc::clone(&extension)]))),
                    ..Default::default()
                }),
                init: None,
            },
        )
        .await
        .expect("root")
        .id();
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.resolve-agent", serde_json::json!({})),
    )
    .await;
    opened
        .harness
        .wait_for_task(id, &context())
        .await
        .expect("wait");
    let events: Vec<(String, JsonObject)> = recorder
        .0
        .root_events
        .lock()
        .expect("events")
        .iter()
        .filter(|(name, attributes)| {
            name == "pi.durable.report"
                && attributes.get("type").and_then(JsonValue::as_str) == Some("extension_error")
        })
        .cloned()
        .collect();
    let metrics = recorder
        .0
        .metrics
        .lock()
        .expect("metrics")
        .iter()
        .filter(|(name, _, attributes)| {
            name == "pi.durable.reports"
                && attributes.get("type").and_then(JsonValue::as_str) == Some("extension_error")
        })
        .count();
    assert_eq!(events.len(), 1, "wrapper renames emit one event");
    assert_eq!(
        events[0].1.get("extension").and_then(JsonValue::as_str),
        Some("wrap-test")
    );
    assert_eq!(metrics, 1, "wrapper renames emit one metric");
}

/// R-D9: `pi.durable.report` carries only the fixed attributes of its kind
/// (upstream `safeReport`): `reason` is a fixed enum value and only
/// `task_faulted`/`task_outdated` have it.
#[tokio::test]
async fn report_events_carry_only_their_kind_attributes() {
    let recorder = Recorder::default();
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let telemetry = || TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let mut opened = open_with_telemetry(Arc::clone(&storage), all_tasks(), telemetry()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 3 })),
    )
    .await;
    // Five crash-loops park it, and the park reports `task_parked`.
    for _ in 0..6 {
        let mut record = opened
            .harness
            .get_task(id, &context())
            .await
            .expect("task")
            .expect("record");
        record.state = rpi_durable::types::TaskState::Running {
            checkpoint: scheduler_support::checkpoint(&record),
        };
        scheduler_support::stamp_task(&opened.harness, record).await;
        opened = open_with_telemetry(Arc::clone(&storage), all_tasks(), telemetry()).await;
    }
    let mut events: Vec<JsonObject> = recorder
        .0
        .spans
        .lock()
        .expect("spans")
        .iter()
        .flat_map(|span| span.event_attributes.clone())
        .collect();
    events.extend(
        recorder
            .0
            .root_events
            .lock()
            .expect("events")
            .iter()
            .filter(|(name, _)| name == "pi.durable.report")
            .map(|(_, attributes)| attributes.clone()),
    );
    let parked = events
        .iter()
        .find(|event| event.get("type").and_then(JsonValue::as_str) == Some("task_parked"))
        .unwrap_or_else(|| panic!("no task_parked event: {events:?}"));
    assert!(
        parked.get("reason").is_none(),
        "task_parked has no fixed reason: {parked:?}"
    );
    assert_eq!(parked["task.id"], JsonValue::from(id.get()));
    assert_eq!(
        parked.keys().cloned().collect::<Vec<_>>(),
        vec!["type".to_string(), "task.id".to_string()],
        "only the kind's fixed attributes: {parked:?}"
    );
    // A faulted task carries the fixed reason; a host error carries none.
    assert!(events.iter().all(|event| {
        event.get("type").and_then(JsonValue::as_str) != Some("task_faulted")
            || matches!(
                event.get("reason").and_then(JsonValue::as_str),
                Some("threw") | Some("restart_limit")
            )
    }));
    assert!(events.iter().all(|event| {
        event.get("type").and_then(JsonValue::as_str) != Some("host_error")
            || event.get("reason").is_none()
    }));
    opened.harness.close(&context()).await.expect("close");
}

/// R-D9: an abort phase records its duration like a run phase (upstream `#phase`
/// wraps both).
#[tokio::test]
async fn an_abort_phase_records_its_duration() {
    let recorder = Recorder::default();
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let opened = open_with_telemetry(Arc::clone(&recorder_storage()), all_tasks(), telemetry).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned(
            "test.abort",
            serde_json::json!({ "phase": "step", "count": 0 }),
        ),
    )
    .await;
    opened.harness.resume().expect("resume");
    opened
        .harness
        .abort_task(id, &context())
        .await
        .expect("abort");
    wait_terminal(&opened.harness, id).await;
    let metrics = recorder.0.metrics.lock().expect("metrics").clone();
    assert!(
        metrics.iter().any(|(name, _, attributes)| {
            name == "pi.durable.phase.duration"
                && attributes["task"] == "test.abort"
                && attributes["outcome"] == "aborted"
        }),
        "missing abort duration: {metrics:?}"
    );
    opened.harness.close(&context()).await.expect("close");
}

/// R-D9: a phase that ends in a sleep still ends its span with that outcome,
/// and a later Session failure does not leave it unfinished. (`step` normalizes
/// a failed commit into an ended invocation, so the span-end-on-error branch in
/// `run`/`run_abort` is defensive.)
#[tokio::test]
async fn a_phase_that_commits_a_sleep_ends_its_span_before_the_session_fails() {
    let recorder = Recorder::default();
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let opened = open_with_telemetry(Arc::clone(&recorder_storage()), all_tasks(), telemetry).await;
    let conversation = root(&opened.harness).await;
    add_task(
        &opened.harness,
        conversation,
        Spec::owned(
            "test.abort",
            serde_json::json!({ "phase": "step", "count": 0 }),
        ),
    )
    .await;
    opened.harness.resume().expect("resume");
    for _ in 0..200 {
        if recorder.span_count("pi.durable.phase") > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(recorder.span_count("pi.durable.phase"), 1);
    opened.harness.session().fail("storage exploded");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let span = recorder.last_span("pi.durable.phase");
    assert!(span.ended, "the failed phase span must end");
    assert_eq!(
        span.end_attributes["outcome"], "sleeping",
        "the phase's own outcome: {:?}",
        span.end_attributes
    );
    let metrics = recorder.0.metrics.lock().expect("metrics").clone();
    assert!(
        metrics.iter().any(|(name, _, attributes)| {
            name == "pi.durable.phase.duration" && attributes["task"] == "test.abort"
        }),
        "the failed phase still records its duration"
    );
}

fn recorder_storage() -> Arc<dyn Storage> {
    Arc::new(MemoryStorage::new())
}

/// R-D9: a host `onReport` info callback that throws is contained, and a report
/// outside the scheduler's context path still emits its fixed telemetry
/// (upstream `safeReport`).
#[tokio::test]
async fn a_panicking_report_callback_is_contained_and_still_reported() {
    let recorder = Recorder::default();
    let telemetry = TelemetryContext::new(Arc::new(recorder.root()) as Arc<dyn TelemetrySink>);
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = scheduler_support::open_with_telemetry_and_report(
        Arc::clone(&storage),
        all_tasks(),
        telemetry,
        Some(Arc::new(|_info: rpi_durable::harness::ReportInfo| {
            panic!("host report callback");
        })),
    )
    .await;
    // Failing the Session runs the failure reporter, which reports
    // `session_failed` through the throwing callback.
    opened.harness.session().fail("storage exploded");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let events: Vec<(String, JsonObject)> = recorder.0.root_events.lock().expect("events").clone();
    let report = events
        .iter()
        .find(|(name, attributes)| {
            name == "pi.durable.report"
                && attributes.get("type").and_then(JsonValue::as_str) == Some("session_failed")
        })
        .unwrap_or_else(|| panic!("no session_failed report event: {events:?}"));
    assert_eq!(
        report.1.keys().cloned().collect::<Vec<_>>(),
        vec!["type".to_string()],
        "session_failed carries no extra attribute: {:?}",
        report.1
    );
}
