//! Scheduler recovery, blocked-definition, and migration tests (port of
//! `packages/durable/test/harness-tasks-recovery.test.ts` @ `4a080252f`).
//!
//! Fixtures stamp durable records directly (simulating a crash between commits)
//! and then reopen a Harness over the same Storage, exercising `open()`'s
//! running→pending rewind, blocked-definition reporting, and reservation
//! migration.

mod scheduler_support;

use std::sync::Arc;

use rpi_durable::harness::{
    BlockedReason, Extension, Harness, Registry, TaskAbort, TaskInspectionState,
};
use rpi_durable::storage::MemoryStorage;
use rpi_durable::types::{Storage, TaskState, TaskStatus};

use scheduler_support::{
    DynamicTask, Spec, add_task, all_tasks, context, open_with, open_with_storage,
    open_with_storage_and_settings, root, stamp_task, terminal_status,
};

/// upstream: "resumes ... task interrupted ... across close/reopen"; a surviving
/// `running` record is rewound to `pending` by `open()`.
#[tokio::test]
async fn rewinds_interrupted_running_to_pending() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 5 })),
    )
    .await;

    // Simulate a crash after the record became `running`: stamp it directly
    // without letting the invocation commit progress.
    let mut record = opened
        .harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task record");
    record.state = TaskState::Running {
        checkpoint: serde_json::json!({ "phase": "step", "count": 1 }),
    };
    stamp_task(&opened.harness, record).await;

    let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let record = reopened
        .harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task record");
    assert_eq!(record.status(), TaskStatus::Pending);
    // The checkpoint survives the rewind.
    assert_eq!(
        scheduler_support::checkpoint(&record),
        serde_json::json!({ "phase": "step", "count": 1 })
    );
    reopened.harness.close(&context()).await.unwrap();
}

/// upstream: "migration at reservation".
#[tokio::test]
async fn migrates_record_at_reservation() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let mut spec = Spec::owned("test.migrate", serde_json::json!({ "seed": 1 }));
    spec.version = 1;
    spec.checkpoint = serde_json::json!({ "phase": "step", "v": 1 });
    let id = add_task(&opened.harness, conversation, spec).await;

    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(
        settled.record.version, 2,
        "migrated to the registered version"
    );
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    assert_eq!(settled.result, serde_json::json!({ "v": 2 }));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "missing definition stays pending/live until registration".
#[tokio::test]
async fn blocked_until_definition_registered() {
    let opened = open_with(Vec::new()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.dynamic", serde_json::json!({})),
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

    opened
        .registry
        .install(Extension {
            name: "dynamic".into(),
            tasks: vec![Arc::new(DynamicTask)],
            ..Extension::default()
        })
        .expect("install");
    opened.harness.resume().unwrap();
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "older record without migration blocked".
#[tokio::test]
async fn older_record_without_migration_is_blocked() {
    let opened = open_with(all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let mut spec = Spec::owned("test.old", serde_json::json!({}));
    spec.version = 1;
    spec.checkpoint = serde_json::json!({ "phase": "step", "v": 1 });
    let id = add_task(&opened.harness, conversation, spec).await;

    let inspection = opened.harness.inspect(&context()).await.unwrap();
    let entry = inspection
        .tasks
        .iter()
        .find(|task| task.record.id == id)
        .expect("live task");
    assert!(
        matches!(
            entry.state,
            TaskInspectionState::Blocked {
                reason: BlockedReason::MigrationFailed,
                ..
            }
        ),
        "state: {:?}",
        entry.state
    );
    assert_eq!(entry.record.status(), TaskStatus::Pending);
    opened.harness.close(&context()).await.unwrap();
}

/// upstream: "orphan marked task whose def vanished".
#[tokio::test]
async fn orphans_abort_marked_task_without_definition() {
    let opened = open_with(Vec::new()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.orphan", serde_json::json!({})),
    )
    .await;

    let result = opened.harness.abort_task(id, &context()).await.unwrap();
    assert_eq!(result, TaskAbort::Marked);
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(terminal_status(&settled.record), Some("orphaned"));
    opened.harness.close(&context()).await.unwrap();
}

// ─── Restart limits (V18-06 FR-E) ─────────────────────────────────────────

async fn stamp_running(
    harness: &Arc<rpi_durable::harness::HarnessImpl>,
    id: rpi_durable::ids::TaskId,
) {
    let mut record = harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task");
    record.state = TaskState::Running {
        checkpoint: scheduler_support::checkpoint(&record),
    };
    stamp_task(harness, record).await;
}

/// R-D7: five consecutive cut-offs park a task; `inspect()` reports the reason.
#[tokio::test]
async fn five_restarts_park_the_task() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 3 })),
    )
    .await;
    stamp_running(&opened.harness, id).await;

    for expected in 1..=5 {
        stamp_running(&opened.harness, id).await;
        let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
        let record = reopened
            .harness
            .get_task(id, &context())
            .await
            .unwrap()
            .expect("task");
        assert_eq!(record.restarts, Some(expected));
    }
    let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let record = reopened
        .harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task");
    assert_eq!(record.status(), TaskStatus::Pending);
    assert!(record.restarts.unwrap_or(0) >= 5);
    let inspection = reopened.harness.inspect(&context()).await.unwrap();
    let entry = inspection
        .tasks
        .iter()
        .find(|task| task.record.id == id)
        .expect("live");
    assert!(
        matches!(
            entry.state,
            TaskInspectionState::Blocked {
                reason: BlockedReason::RestartLimit,
                ..
            }
        ),
        "state: {:?}",
        entry.state
    );
    reopened.harness.close(&context()).await.unwrap();
}

/// R-D7: `Infinity` never parks.
#[tokio::test]
async fn infinite_restart_limit_never_parks() {
    use rpi_durable::harness::{HarnessSettings, RestartLimit};
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let settings = HarnessSettings {
        restart_limit: Some(RestartLimit::Infinite),
        ..Default::default()
    };
    let opened =
        open_with_storage_and_settings(Arc::clone(&storage), all_tasks(), Some(settings.clone()))
            .await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 3 })),
    )
    .await;
    stamp_running(&opened.harness, id).await;
    for _ in 0..10 {
        stamp_running(&opened.harness, id).await;
        let reopened = open_with_storage_and_settings(
            Arc::clone(&storage),
            all_tasks(),
            Some(settings.clone()),
        )
        .await;
        let _ = reopened;
    }
    let reopened =
        open_with_storage_and_settings(Arc::clone(&storage), all_tasks(), Some(settings)).await;
    let record = reopened
        .harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task");
    assert!(record.restarts.unwrap_or(0) >= 10);
    let inspection = reopened.harness.inspect(&context()).await.unwrap();
    let entry = inspection
        .tasks
        .iter()
        .find(|task| task.record.id == id)
        .expect("live");
    assert!(!matches!(
        entry.state,
        TaskInspectionState::Blocked {
            reason: BlockedReason::RestartLimit,
            ..
        }
    ));
    reopened.harness.close(&context()).await.unwrap();
}

/// R-D7: a committed state clears the restart count.
#[tokio::test]
async fn commit_clears_the_restart_count() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 1 })),
    )
    .await;
    stamp_running(&opened.harness, id).await;
    let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let record = reopened
        .harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task");
    assert_eq!(record.restarts, Some(1));
    reopened.harness.resume().unwrap();
    let settled = reopened
        .harness
        .wait_for_task(id, &context())
        .await
        .unwrap();
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    assert_eq!(settled.record.restarts, None);
    reopened.harness.close(&context()).await.unwrap();
}

/// R-D7: `retryTask()` unparks a task and it runs again.
#[tokio::test]
async fn retry_task_unparks() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 1 })),
    )
    .await;
    stamp_running(&opened.harness, id).await;
    for _ in 0..5 {
        let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
        let _ = reopened;
    }
    let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    reopened
        .harness
        .retry_task(id, &context())
        .await
        .expect("retry");
    let settled = reopened
        .harness
        .wait_for_task(id, &context())
        .await
        .unwrap();
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    reopened.harness.close(&context()).await.unwrap();
}

/// R-D7: an abort handler cut off at the limit faults instead of running again.
#[tokio::test]
async fn abort_handler_at_the_limit_faults() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let opened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.count", serde_json::json!({ "phases": 3 })),
    )
    .await;
    let mut record = opened
        .harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task");
    record.state = TaskState::Running {
        checkpoint: scheduler_support::checkpoint(&record),
    };
    record.abort_requested = true;
    record.restarts = Some(5);
    stamp_task(&opened.harness, record).await;
    let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    reopened.harness.resume().unwrap();
    let settled = reopened
        .harness
        .wait_for_task(id, &context())
        .await
        .unwrap();
    assert_eq!(terminal_status(&settled.record), Some("faulted"));
    match settled.record.state {
        TaskState::Terminal {
            outcome: rpi_durable::types::TaskOutcome::Faulted { error },
        } => assert!(error.message.contains("cut off"), "got {}", error.message),
        other => panic!("unexpected state {other:?}"),
    }
    reopened.harness.close(&context()).await.unwrap();
}

/// R-D7: a normal close writes a stopped invocation back as pending without
/// counting a restart.
#[tokio::test]
async fn normal_close_does_not_count_a_restart() {
    struct BlockingTask;

    #[async_trait::async_trait]
    impl rpi_durable::harness::TaskDefinition for BlockingTask {
        fn name(&self) -> &str {
            "test.blocking"
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
            let until = runtime.now()? + 3_600_000.0;
            runtime
                .sleep_until(until, serde_json::json!({ "phase": "done" }), context)
                .await
        }
    }

    let dir = std::env::temp_dir().join(format!(
        "rpi-durable-close-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let path = dir.to_string_lossy().into_owned();
    let open_jsonl = |path: &str| {
        let path = path.to_string();
        let context = context();
        async move {
            use rpi_durable::fs::{FileSystem, StdFileSystem};
            use rpi_durable::storage::{JsonlStorage, JsonlStorageOptions};
            let fs: Arc<dyn FileSystem> = Arc::new(StdFileSystem::new(&path));
            let storage = JsonlStorage::open(&path, fs, JsonlStorageOptions::default(), &context)
                .await
                .expect("jsonl");
            Arc::new(storage) as Arc<dyn Storage>
        }
    };
    let storage = open_jsonl(&path).await;
    let opened = open_with_storage(Arc::clone(&storage), vec![Arc::new(BlockingTask)]).await;
    let conversation = root(&opened.harness).await;
    let id = add_task(
        &opened.harness,
        conversation,
        Spec::owned("test.blocking", serde_json::json!({})),
    )
    .await;
    opened.harness.resume().unwrap();
    for _ in 0..100 {
        let record = opened
            .harness
            .get_task(id, &context())
            .await
            .unwrap()
            .expect("task");
        if record.status() == TaskStatus::Sleeping {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    opened.harness.close(&context()).await.unwrap();

    let storage = open_jsonl(&path).await;
    let reopened = open_with_storage(Arc::clone(&storage), vec![Arc::new(BlockingTask)]).await;
    let record = reopened
        .harness
        .get_task(id, &context())
        .await
        .unwrap()
        .expect("task");
    assert_eq!(record.status(), TaskStatus::Sleeping);
    assert_eq!(record.restarts, None, "a close is not a restart");
    reopened.harness.close(&context()).await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}
