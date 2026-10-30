//! Telemetry span/trace integration tests for V18-06 FR-F R3.
//!
//! A recording sink implements the adapter contract: spans carry parent ids,
//! start/end attributes, events, links and status; carriers are injected into
//! durable task and submission records and extracted by later invocations.

mod scheduler_support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_durable::chord::context::{TELEMETRY_CONTEXT_KEY, with_context_value};
use rpi_durable::chord::json::{JsonObject, JsonValue};
use rpi_durable::chord::telemetry::{
    TelemetryCarrier, TelemetryContext, TelemetrySink, TelemetrySpanSink,
};
use rpi_durable::ids::{ConversationId, TaskId};
use rpi_durable::session::{TaskCreate, TaskOwnership, TaskTrace, commit_fn};
use rpi_durable::storage::MemoryStorage;
use rpi_durable::types::{Storage, TaskStatus};
use scheduler_support::{Spec, add_task, all_tasks, context, open_with_telemetry, root};

#[derive(Default)]
struct State {
    spans: Mutex<Vec<Span>>,
    metrics: Mutex<Vec<(String, f64, JsonObject)>>,
    next: AtomicU64,
}

struct Span {
    id: u64,
    parent: Option<u64>,
    name: String,
    attributes: JsonObject,
    links: Vec<TelemetryCarrier>,
    events: Vec<String>,
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

    fn add_event(&self, name: &str, _attributes: JsonObject) {
        self.with_span(|span| span.events.push(name.to_string()));
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
    assert!(
        recorder
            .0
            .metrics
            .lock()
            .expect("metrics")
            .iter()
            .any(|(name, _, attributes)| name == "pi.durable.phase.duration"
                && attributes["task"] == "test.memo")
    );
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
