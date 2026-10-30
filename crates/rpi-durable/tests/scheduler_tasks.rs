//! Scheduler task lifecycle tests (port of
//! `packages/durable/test/harness-tasks.test.ts` @ `4a080252f`).
//!
//! Every case drives the real [`rpi_durable::harness::TaskScheduler`] through a
//! Harness opened over memory storage. Upstream `describe`/`it` anchors are
//! cited per test.

mod scheduler_support;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rpi_durable::harness::{
    BlockedReason, Extension, Registry, ReportInfo, ReportKind, TaskAbort, TaskInspectionState,
};
use rpi_durable::storage::MemoryStorage;
use rpi_durable::types::{Storage, TaskStatus};

use scheduler_support::{
    Spec, add_task, all_tasks, context, open_with, open_with_storage,
    open_with_storage_settings_and_report, root, terminal_status,
};

/// upstream: "continues one invocation through checkpoint progress".
#[tokio::test]
async fn continues_progress_and_completes() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 3 })),
    )
    .await;

    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(settled.result, serde_json::json!({ "count": 3 }));
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "faults a ... throwing phase".
#[tokio::test]
async fn throwing_phase_faults() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.throw", serde_json::json!({})),
    )
    .await;

    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(terminal_status(&settled.record), Some("faulted"));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "faults a phase without durable progress".
#[tokio::test]
async fn no_progress_faults() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.noprogress", serde_json::json!({})),
    )
    .await;

    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(terminal_status(&settled.record), Some("faulted"));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "keeps a committed terminal outcome when handler throws afterwards".
#[tokio::test]
async fn committed_terminal_survives_later_throw() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.terminal-throw", serde_json::json!({})),
    )
    .await;

    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    assert_eq!(settled.result, serde_json::json!({ "kept": true }));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "resumes a waiting task once every task in `on` terminal (any outcome)".
#[tokio::test]
async fn waits_all_settled() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let first = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 1 })),
    )
    .await;
    let second = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 2 })),
    )
    .await;
    let mut waiter = Spec::owned(
        "test.wait",
        serde_json::json!({ "on": [first.get(), second.get()], "policy": "allSettled" }),
    );
    waiter.checkpoint = serde_json::json!({ "phase": "wait", "waited": false });
    let waiter_id = add_task(&opened.harness, conversation, waiter).await;

    let settled = opened
        .harness
        .wait_for_task(waiter_id, &context())
        .await
        .unwrap();
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    assert_eq!(settled.result, serde_json::json!({ "done": true }));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "stamps startedAt at first run / endedAt at terminal; keeps them through reopen".
#[tokio::test]
async fn stamps_times_and_survives_reopen() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 2 })),
    )
    .await;

    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    let started = settled.record.started_at;
    let ended = settled.record.ended_at;
    assert!(started.is_some(), "startedAt stamped");
    assert!(ended.is_some(), "endedAt stamped");

    // Reopen a second Harness over the same Storage (a crash-free restart).
    let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let record = reopened
        .harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task record");
    assert_eq!(record.started_at, started);
    assert_eq!(record.ended_at, ended);
    assert_eq!(terminal_status(&record), Some("completed"));
    reopened.harness.close(&context()).await.unwrap();
}

/// upstream: "keeps memos until terminal".
#[tokio::test]
async fn keeps_memos_across_phases() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.memo", serde_json::json!({})),
    )
    .await;

    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(settled.result, serde_json::json!(7));
    assert!(
        settled.record.memos.is_none(),
        "memos retired with the outcome"
    );
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "resumes abort work after ... abort"; commit the abort mark and run the handler.
#[tokio::test]
async fn abort_runs_handler_to_terminal() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.abort", serde_json::json!({})),
    )
    .await;

    let result = opened.harness.abort_task(id, &context()).await.unwrap();
    assert_eq!(result, TaskAbort::Marked);
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(terminal_status(&settled.record), Some("aborted"));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "an abort handler that returns without an outcome faults the task".
#[tokio::test]
async fn abort_without_outcome_faults() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.abort-no-outcome", serde_json::json!({})),
    )
    .await;

    let _ = opened.harness.abort_task(id, &context()).await.unwrap();
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(terminal_status(&settled.record), Some("faulted"));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "marks the sibling of a failed task in a `failFast` wait".
#[tokio::test]
async fn fail_fast_marks_siblings_and_settles() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.fanout", serde_json::json!({})),
    )
    .await;

    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    assert_eq!(settled.result, serde_json::json!({ "fanout": true }));
    // The slow sibling is abort-marked by the cascade, so the scope drains.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        opened.harness.wait_for_idle(&context()),
    )
    .await
    .expect("idle")
    .unwrap();
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "blocked tasks ... missing definition stays pending/live until registration".
#[tokio::test]
async fn inspect_reports_missing_definition() {
    let opened = open_with(Vec::new()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 1 })),
    )
    .await;

    let inspection = opened.harness.inspect(&context()).await.unwrap();
    let entry = inspection
        .tasks
        .iter()
        .find(|task| task.record.id == id)
        .expect("live task");
    assert!(matches!(
        entry.state,
        TaskInspectionState::Blocked {
            reason: BlockedReason::MissingTask,
            ..
        }
    ));
    assert_eq!(entry.record.status(), TaskStatus::Pending);
    opened.harness.close(&context()).await.unwrap();
}

// ─── abortOwned/ownedTasks (V18-06 FR-I) ─────────────────────────────────

struct ChildTask;

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for ChildTask {
    fn name(&self) -> &str {
        "test.child"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> rpi_durable::chord::json::JsonValue {
        serde_json::json!({ "phase": "wait" })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        _context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        runtime.signal().cancelled().await;
        Err(rpi_durable::session::SessionError::Aborted)
    }
    async fn abort(
        &self,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        runtime
            .commit(
                scheduler_support::commit_next(rpi_durable::harness::NextTaskState::Terminal {
                    outcome: rpi_durable::types::TaskOutcome::Aborted {
                        reason: Some("owned".to_string()),
                        result: None,
                    },
                }),
                context,
            )
            .await
    }
}

struct ParentTask;

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for ParentTask {
    fn name(&self) -> &str {
        "test.parent"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> rpi_durable::chord::json::JsonValue {
        serde_json::json!({ "phase": "spawn" })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        let child: rpi_durable::harness::AnyTask = Arc::new(ChildTask);
        let child_id = runtime
            .create_task(
                child,
                rpi_durable::chord::json::JsonValue::Null,
                rpi_durable::session::TaskOwnership::Task(runtime.task_id()),
                false,
                context,
            )
            .await?;
        let owned = runtime.owned_tasks(context).await?;
        let owned_before = owned.iter().filter(|task| task.id == child_id).count();
        runtime.abort_owned(child_id, context).await?;
        let child_record = runtime
            .get_task(child_id, context)
            .await?
            .expect("child record");
        let child_terminal = matches!(
            child_record.state,
            rpi_durable::types::TaskState::Terminal { .. }
        );
        runtime
            .commit(
                scheduler_support::commit_next(rpi_durable::harness::NextTaskState::Terminal {
                    outcome: rpi_durable::types::TaskOutcome::Completed {
                        result: serde_json::json!({
                            "ownedBefore": owned_before,
                            "childTerminal": child_terminal,
                        }),
                    },
                }),
                context,
            )
            .await
    }
}

/// R-D14: `ownedTasks()` copies the direct owned live tasks and `abortOwned()`
/// waits until the child is terminal.
#[tokio::test]
async fn abort_owned_waits_for_the_child_and_lists_it_first() {
    let opened = open_with(vec![Arc::new(ParentTask), Arc::new(ChildTask)]).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.parent", serde_json::json!({})),
    )
    .await;
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(settled.result["ownedBefore"], serde_json::json!(1));
    assert_eq!(settled.result["childTerminal"], serde_json::json!(true));
    opened.harness.close(&context()).await.unwrap();
}

/// A parent that creates a conversation-owned task (not its child) and tries to
/// abort it.
struct NegativeParentTask;

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for NegativeParentTask {
    fn name(&self) -> &str {
        "test.negative-parent"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> rpi_durable::chord::json::JsonValue {
        serde_json::json!({ "phase": "spawn" })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        let child: rpi_durable::harness::AnyTask = Arc::new(ChildTask);
        let child_id = runtime
            .create_task(
                child,
                rpi_durable::chord::json::JsonValue::Null,
                rpi_durable::session::TaskOwnership::Conversation(runtime.conversation_id()),
                false,
                context,
            )
            .await?;
        let owned = runtime.owned_tasks(context).await?;
        let not_owned = owned.iter().all(|task| task.id != child_id);
        let error = runtime
            .abort_owned(child_id, context)
            .await
            .expect_err("a conversation-owned task is not owned by this task");
        runtime
            .commit(
                scheduler_support::commit_next(rpi_durable::harness::NextTaskState::Terminal {
                    outcome: rpi_durable::types::TaskOutcome::Completed {
                        result: serde_json::json!({
                            "notOwned": not_owned,
                            "error": error.to_string(),
                        }),
                    },
                }),
                context,
            )
            .await
    }
}

/// R-D14: `abortOwned()` rejects a task that is not directly owned, and
/// `ownedTasks()` never lists it.
#[tokio::test]
async fn abort_owned_rejects_a_task_it_does_not_own() {
    let opened = open_with(vec![Arc::new(NegativeParentTask), Arc::new(ChildTask)]).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.negative-parent", serde_json::json!({})),
    )
    .await;
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(settled.result["notOwned"], serde_json::json!(true));
    assert!(
        settled.result["error"]
            .as_str()
            .is_some_and(|error| error.contains("is not owned by task")),
        "got {}",
        settled.result
    );
    opened.harness.close(&context()).await.unwrap();
}

struct SleepOwner;

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for SleepOwner {
    fn name(&self) -> &str {
        "test.sleep-owner"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> rpi_durable::chord::json::JsonValue {
        serde_json::json!({ "phase": "sleep" })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        let until = runtime.now()? + 3_600_000.0;
        runtime
            .sleep_until(until, serde_json::json!({ "phase": "done" }), context)
            .await
    }
}

struct CountingTask(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for CountingTask {
    fn name(&self) -> &str {
        "test.counting"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> rpi_durable::chord::json::JsonValue {
        serde_json::json!({ "phase": "step" })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        _task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        runtime
            .commit(
                scheduler_support::commit_next(rpi_durable::harness::NextTaskState::Terminal {
                    outcome: rpi_durable::types::TaskOutcome::Completed {
                        result: serde_json::json!({}),
                    },
                }),
                context,
            )
            .await
    }
}

/// R-D14: a cancellation intent above a task parks it before any run phase.
#[tokio::test]
async fn a_cancelled_owner_prevents_below_run_phases() {
    let runs = Arc::new(AtomicUsize::new(0));
    let opened = open_with(vec![
        Arc::new(SleepOwner),
        Arc::new(CountingTask(Arc::clone(&runs))),
    ])
    .await;
    let conversation = root(&opened.harness).await;
    let owner = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.sleep-owner", serde_json::json!({})),
    )
    .await;
    let child = add_task(
        &opened.harness,
        conversation,
        Spec::child("test.counting", serde_json::json!({}), owner),
    )
    .await;
    let mut record = opened
        .harness
        .get_task(owner, &context())
        .await
        .unwrap()
        .expect("owner");
    record.abort_requested = true;
    scheduler_support::stamp_task(&opened.harness, record).await;

    opened.harness.resume().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "a child below a cancelled owner must not run"
    );
    let child_record = opened
        .harness
        .get_task(child, &context())
        .await
        .unwrap()
        .expect("child");
    assert!(matches!(
        child_record.state,
        rpi_durable::types::TaskState::Terminal { .. }
            | rpi_durable::types::TaskState::Completing { .. }
    ));
    opened.harness.close(&context()).await.unwrap();
}

struct SlowTask(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl rpi_durable::harness::TaskDefinition for SlowTask {
    fn name(&self) -> &str {
        "test.slow"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> rpi_durable::chord::json::JsonValue {
        serde_json::json!({ "phase": "step", "count": 0 })
    }
    async fn run_phase(
        &self,
        _phase: &str,
        task: rpi_durable::harness::RunningTask,
        runtime: &dyn rpi_durable::harness::TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> rpi_durable::session::SessionResult<()> {
        let count = task
            .checkpoint()
            .and_then(|checkpoint| checkpoint.get("count"))
            .and_then(rpi_durable::chord::json::JsonValue::as_u64)
            .unwrap_or(0);
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let next = if count + 1 >= 6 {
            rpi_durable::harness::NextTaskState::Terminal {
                outcome: rpi_durable::types::TaskOutcome::Completed {
                    result: serde_json::json!({ "count": count + 1 }),
                },
            }
        } else {
            rpi_durable::harness::NextTaskState::Running {
                checkpoint: serde_json::json!({ "phase": "step", "count": count + 1 }),
            }
        };
        runtime
            .commit(scheduler_support::commit_next(next), context)
            .await
    }
}

/// R-D7: an invocation under an uninstalled definition reports `task_outdated`
/// once, not once per phase.
#[tokio::test]
async fn an_outdated_definition_reports_once_per_invocation() {
    let phases = Arc::new(AtomicUsize::new(0));
    let reports = Arc::new(Mutex::new(Vec::<ReportKind>::new()));
    let sink = Arc::clone(&reports);
    let opened = open_with_storage_settings_and_report(
        Arc::new(MemoryStorage::new()),
        vec![Arc::new(SlowTask(Arc::clone(&phases)))],
        None,
        Some(Arc::new(move |info: ReportInfo| {
            sink.lock().expect("reports").push(info.kind);
        })),
    )
    .await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.slow", serde_json::json!({})),
    )
    .await;
    opened.harness.resume().unwrap();
    for _ in 0..200 {
        if phases.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    opened.registry.uninstall(&Extension {
        name: "test".to_string(),
        ..Extension::default()
    });
    let _ = opened.harness.wait_for_task(id, &context()).await.unwrap();
    let outdated = reports
        .lock()
        .expect("reports")
        .iter()
        .filter(|kind| **kind == ReportKind::TaskOutdated)
        .count();
    assert_eq!(outdated, 1, "got {:?}", reports.lock().expect("reports"));
    opened.harness.close(&context()).await.unwrap();
}
