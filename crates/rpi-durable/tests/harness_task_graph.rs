//! Harness task-graph integration tests (port of
//! `packages/durable/test/harness-task-graph.test.ts` @ `b4afdddf`).
//!
//! Scoped to what does not need the scheduler: tasks are created directly on the
//! Session line, and the graph is observed through the Harness composition.

mod harness_support;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use harness_support::{context, open_recording};
use rpi_durable::JsonValue;
use rpi_durable::harness::{HarnessImpl, RootOptions};
use rpi_durable::ids::{ConversationId, TaskId};
use rpi_durable::session::{TaskCreate, TaskOwnership, commit_fn};
use rpi_durable::types::{TaskOutcome, TaskRecord, TaskState};

async fn root(harness: &Arc<HarnessImpl>) -> ConversationId {
    harness
        .root(&context(), RootOptions::default())
        .await
        .expect("root")
        .id()
}

async fn create_task(
    harness: &Arc<HarnessImpl>,
    conversation: ConversationId,
    kind: &str,
) -> TaskId {
    let kind = kind.to_string();
    let change = commit_fn(move |tx| {
        let kind = kind.clone();
        Box::pin(async move {
            tx.create_task(TaskCreate {
                kind,
                version: 1,
                input: JsonValue::Null,
                checkpoint: serde_json::json!({ "phase": "prepare" }),
                ownership: TaskOwnership::Conversation(conversation),
                conversation_id: Some(conversation),
                background: false,
                abandon_on_restart: false,
                trace: Default::default(),
                links: Vec::new(),
            })
            .await
        })
    });
    harness
        .session()
        .commit(change, &context())
        .await
        .expect("create task")
}

async fn record(harness: &Arc<HarnessImpl>, id: TaskId) -> TaskRecord {
    harness
        .session()
        .storage()
        .task(id, &context())
        .await
        .expect("task")
        .expect("present")
        .as_ref()
        .clone()
}

async fn set_state(harness: &Arc<HarnessImpl>, id: TaskId, state: TaskState) {
    let record = record(harness, id).await;
    let change = commit_fn(move |tx| {
        let next = TaskRecord {
            state: state.clone(),
            ..record.clone()
        };
        Box::pin(async move { tx.set_task(next) })
    });
    harness
        .session()
        .commit(change, &context())
        .await
        .expect("set task");
}

fn node(harness_value: &JsonValue, id: TaskId) -> JsonValue {
    harness_value["tasks"][id.get().to_string()].clone()
}

#[tokio::test]
async fn follows_a_live_task_through_status_changes() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root(&harness).await;
    let task = create_task(&harness, conversation, "pi.generation").await;
    let graph = harness.task_graph(&context).await.expect("graph");
    let pending = node(&graph.value(), task);
    assert_eq!(pending["state"]["status"], "pending");
    assert_eq!(pending["kind"], "pi.generation");
    assert_eq!(pending["conversationId"], conversation.get());

    set_state(
        &harness,
        task,
        TaskState::Waiting {
            checkpoint: serde_json::json!({ "phase": "tools" }),
            on: vec![],
            policy: rpi_durable::types::JoinPolicy::AllSettled,
        },
    )
    .await;
    let waiting = node(&graph.value(), task);
    assert_eq!(waiting["state"]["status"], "waiting");
    assert_eq!(waiting["state"]["phase"], "tools");

    set_state(
        &harness,
        task,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed {
                result: JsonValue::Null,
            },
        },
    )
    .await;
    // A terminal task leaves the graph.
    assert!(node(&graph.value(), task).is_null());
    graph.dispose();
}

#[tokio::test]
async fn abort_marks_and_owned_conversations_appear_in_the_graph() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root(&harness).await;
    let task = create_task(&harness, conversation, "pi.generation").await;
    // A conversation owned by the task in the same commit.
    let change = commit_fn(move |tx| {
        Box::pin(async move {
            Ok(tx
                .create_conversation(rpi_durable::session::ConversationOwnership::Task(task))
                .await?
                .id)
        })
    });
    let owned = harness
        .session()
        .commit(change, &context)
        .await
        .expect("owned conversation");
    let record = record(&harness, task).await;
    let marked = TaskRecord {
        abort_requested: true,
        ..record
    };
    let change = commit_fn(move |tx| {
        let marked = marked.clone();
        Box::pin(async move { tx.set_task(marked) })
    });
    harness
        .session()
        .commit(change, &context)
        .await
        .expect("mark abort");
    let graph = harness.task_graph(&context).await.expect("graph");
    let value = node(&graph.value(), task);
    assert_eq!(value["abortRequested"], true);
    assert_eq!(value["conversations"], serde_json::json!([owned.get()]));
    graph.dispose();
}

#[tokio::test]
async fn watch_reports_the_latest_committed_revision() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root(&harness).await;
    let task = create_task(&harness, conversation, "pi.generation").await;
    let watch = harness.watch_task_graph(&context).await.expect("watch");
    let log: Arc<Mutex<Vec<JsonValue>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&log);
    let listener = rpi_durable::session::watch_listener(move |call, _context| {
        let value = match call {
            rpi_durable::session::WatchCall::Value { value, .. } => value,
            rpi_durable::session::WatchCall::Failed { .. } => None,
        };
        let captured = Arc::clone(&captured);
        Box::pin(async move {
            captured
                .lock()
                .expect("log")
                .push(value.map(JsonValue::Object).unwrap_or(JsonValue::Null));
            Ok(())
        })
    });
    watch.start(listener).expect("start");
    set_state(
        &harness,
        task,
        TaskState::Running {
            checkpoint: serde_json::json!({ "phase": "request" }),
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let latest = log
        .lock()
        .expect("log")
        .last()
        .cloned()
        .unwrap_or(JsonValue::Null);
    assert_eq!(node(&latest, task)["state"]["status"], "running");
    assert_eq!(node(&latest, task)["state"]["phase"], "request");
    let _ = watch.stop().await;
}

struct GraphObserver {
    values: Arc<Mutex<Vec<rpi_durable::harness::task_graph::TaskGraph>>>,
    ends: Arc<Mutex<Vec<rpi_durable::session::WatchEnd>>>,
    panic_on_advance: bool,
}

impl rpi_durable::session::Observer<rpi_durable::harness::task_graph::TaskGraph> for GraphObserver {
    fn advance(
        &self,
        call: rpi_durable::session::WatchCall<rpi_durable::harness::task_graph::TaskGraph>,
        _context: &rpi_durable::chord::context::Context,
    ) {
        if self.panic_on_advance {
            panic!("observer failure fixture");
        }
        if let rpi_durable::session::WatchCall::Value { value, .. } = call {
            self.values.lock().expect("values").push(value);
        }
    }

    fn end(&self, end: rpi_durable::session::WatchEnd) {
        self.ends.lock().expect("ends").push(end);
    }
}

#[tokio::test]
async fn synchronous_observers_attach_atomically_detach_and_close_as_failed() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root(&harness).await;
    let task = create_task(&harness, conversation, "pi.generation").await;
    let values = Arc::new(Mutex::new(Vec::new()));
    let ends = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::new(GraphObserver {
        values: Arc::clone(&values),
        ends: Arc::clone(&ends),
        panic_on_advance: false,
    });
    let observation = harness
        .observe_task_graph(observer, &context)
        .await
        .expect("observe");
    assert!(
        observation
            .value
            .tasks
            .contains_key(&task.get().to_string())
    );
    set_state(
        &harness,
        task,
        TaskState::Running {
            checkpoint: serde_json::json!({ "phase": "request" }),
        },
    )
    .await;
    assert_eq!(values.lock().expect("values").len(), 1);
    harness.close(&context).await.expect("close");
    assert_eq!(
        ends.lock().expect("ends").as_slice(),
        &[rpi_durable::session::WatchEnd::Failed {
            error: "Session is closed".to_string(),
        }]
    );
    observation.detach.unsubscribe();
}

#[tokio::test]
async fn a_panicking_task_graph_observer_reports_listener_error_and_detaches() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root(&harness).await;
    let task = create_task(&harness, conversation, "pi.generation").await;
    let reports = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&reports);
    harness
        .session()
        .set_listener_reporter(Arc::new(move |message, _| {
            captured.lock().expect("reports").push(message);
        }));
    let ends = Arc::new(Mutex::new(Vec::new()));
    let observation = harness
        .observe_task_graph(
            Arc::new(GraphObserver {
                values: Arc::new(Mutex::new(Vec::new())),
                ends: Arc::clone(&ends),
                panic_on_advance: true,
            }),
            &context,
        )
        .await
        .expect("observe");
    set_state(
        &harness,
        task,
        TaskState::Running {
            checkpoint: serde_json::json!({ "phase": "request" }),
        },
    )
    .await;
    assert!(harness.session().failure().is_none());
    assert!(reports.lock().expect("reports")[0].contains("listener_error"));
    assert!(matches!(
        ends.lock().expect("ends").as_slice(),
        [rpi_durable::session::WatchEnd::ListenerError { .. }]
    ));
    observation.detach.unsubscribe();
}

#[tokio::test]
async fn a_noop_commit_publishes_no_revision() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root(&harness).await;
    create_task(&harness, conversation, "pi.generation").await;
    let graph = harness.task_graph(&context).await.expect("graph");
    let value = graph.value();
    let change = commit_fn(|_tx| Box::pin(async move { Ok::<(), rpi_durable::SessionError>(()) }));
    harness
        .session()
        .commit(change, &context)
        .await
        .expect("noop");
    assert_eq!(graph.value(), value);
    graph.dispose();
}

mod scheduler_support;

use rpi_durable::harness::BlockedReason;
use rpi_durable::harness::{Extension, Harness, Registry};
use scheduler_support::{
    FakeTask, Spec, add_task, all_tasks, context as scheduler_context, open_with, stamp_task,
};

fn blocked(harness_value: &JsonValue, id: TaskId) -> Option<BlockedReason> {
    let node = &harness_value["tasks"][id.get().to_string()];
    node.get("blocked")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// R-D7: the task graph derives blocked reasons and follows registry changes
/// without a commit.
#[tokio::test]
async fn the_graph_reports_missing_and_registered_definitions() {
    let opened = open_with(all_tasks()).await;
    let conversation = scheduler_support::root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.missing", serde_json::json!({})),
    )
    .await;

    let state = opened
        .harness
        .task_graph(&scheduler_context())
        .await
        .expect("graph");
    assert_eq!(
        blocked(&state.value(), id),
        Some(BlockedReason::MissingTask)
    );
    state.dispose();

    opened
        .registry
        .install(Extension {
            name: "missing".into(),
            tasks: vec![Arc::new(FakeTask("test.missing"))],
            ..Extension::default()
        })
        .expect("install definition");

    let mut cleared = false;
    for _ in 0..100 {
        let state = opened
            .harness
            .task_graph(&scheduler_context())
            .await
            .expect("graph");
        if blocked(&state.value(), id).is_none() {
            cleared = true;
            state.dispose();
            break;
        }
        state.dispose();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(cleared, "blocked reason did not follow the registry");
    opened
        .harness
        .close(&scheduler_context())
        .await
        .expect("close");
}

/// R-D7: the restart limit ends the task instead of parking it for retry.
#[tokio::test]
async fn the_graph_reports_the_restart_limit_as_terminal() {
    let opened = open_with(all_tasks()).await;
    let conversation = scheduler_support::root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 5 })),
    )
    .await;
    let mut record = opened
        .harness
        .get_task(id, &scheduler_context())
        .await
        .expect("task")
        .expect("record");
    record.restarts = Some(5);
    stamp_task(&opened.harness, record).await;

    let state = opened
        .harness
        .task_graph(&scheduler_context())
        .await
        .expect("graph");
    assert_eq!(blocked(&state.value(), id), None);
    state.dispose();
    let settled = opened
        .harness
        .wait_for_task(id, &scheduler_context())
        .await
        .expect("terminal task");
    assert!(matches!(
        settled.record.state,
        rpi_durable::types::TaskState::Terminal { .. }
    ));
    // A terminal task must not keep the host awake. Reconcile work from the
    // stamping commit may still be in flight; wait for it to land.
    for _ in 0..100 {
        if !opened.harness.wake().active {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(
        !opened.harness.wake().active,
        "a terminal task must not keep the host awake"
    );

    opened
        .harness
        .close(&scheduler_context())
        .await
        .expect("close");
}

/// R-D5/D7: a persisted sleeping task is present when the graph is first
/// mounted after a reopen.
#[tokio::test]
async fn a_sleeping_task_is_in_the_first_graph_mount() {
    let opened = open_with(all_tasks()).await;
    let conversation = scheduler_support::root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 2 })),
    )
    .await;
    let mut record = opened
        .harness
        .get_task(id, &scheduler_context())
        .await
        .expect("task")
        .expect("record");
    record.state = TaskState::Sleeping {
        checkpoint: serde_json::json!({ "phase": "step", "count": 1 }),
        until: 42_000.0,
    };
    stamp_task(&opened.harness, record).await;

    let state = opened
        .harness
        .task_graph(&scheduler_context())
        .await
        .expect("graph");
    let node = &state.value()["tasks"][id.get().to_string()];
    assert_eq!(node["state"]["status"], "sleeping");
    assert_eq!(node["state"]["until"], serde_json::json!(42_000.0));
    state.dispose();
    opened
        .harness
        .close(&scheduler_context())
        .await
        .expect("close");
}

/// R-D7: an older record under a newer definition without a migration, and a
/// newer record under an older definition, both surface in the graph.
#[tokio::test]
async fn the_graph_reports_too_old_and_missing_migration() {
    let opened = open_with(all_tasks()).await;
    let conversation = scheduler_support::root(&opened.harness).await;
    let mut newer = Spec::owned("test.count", serde_json::json!({ "phases": 1 }));
    newer.version = 2;
    let too_old = add_task(&opened.harness, conversation, newer).await;
    let no_migration = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.old", serde_json::json!({})),
    )
    .await;
    let state = opened
        .harness
        .task_graph(&scheduler_context())
        .await
        .expect("graph");
    let value = state.value();
    assert_eq!(blocked(&value, too_old), Some(BlockedReason::TaskTooOld));
    assert_eq!(
        blocked(&value, no_migration),
        Some(BlockedReason::MigrationFailed)
    );
    state.dispose();
    opened
        .harness
        .close(&scheduler_context())
        .await
        .expect("close");
}
