//! Crash-survival helper for the SQLite backend.
//!
//! Modes:
//! - `crash` (default): opens durable storage, commits one entry, writes a
//!   ready marker and then sleeps until killed (V17-13 FR-E R4 `kill -9`
//!   anchor).
//! - `migrate`: creates a version 1 database with a task row, writes the
//!   marker and starts the version 2 migration with an extra slow statement so
//!   the parent can kill the process mid-migration (V18-05 FR-A R3).

use rpi_durable::chord::context::BACKGROUND_CONTEXT;
use rpi_durable::harness::{
    AnyTask, Extension, HarnessOptions, NextTaskState, Registry, RegistryImpl, RunningTask,
    TaskDefinition, TaskRuntime, builtin_tasks, open_harness,
};
use rpi_durable::session::{SessionResult, TaskCreate, TaskOwnership, commit_fn};
use rpi_durable::storage::{
    NodeSqliteStorageOptions, SqliteDatabaseOptions, SqliteValue, apply_sqlite_migrations,
    open_node_sqlite_database, open_node_sqlite_storage, sqlite_migrations,
};
use rpi_durable::types::{ConversationRecord, EntryRecord, Storage, StorageWrite};
use rpi_durable::{EntryId, ROOT_CONVERSATION_ID, id_from_number};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("database path");
    let marker = args.next().expect("ready marker path");
    let mode = args.next().unwrap_or_else(|| "crash".to_string());
    if mode == "migrate" {
        migrate_then_sleep(&path, &marker).await;
        return;
    }
    if mode == "harness" {
        harness_then_sleep(&path, &marker).await;
        return;
    }
    let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
        .await
        .expect("open storage");
    storage
        .commit(
            &[StorageWrite::Conversation {
                value: ConversationRecord {
                    id: ROOT_CONVERSATION_ID,
                    parent: None,
                    owner: None,
                },
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("root commit");
    let entry_id: EntryId = id_from_number(storage.mint_id().await.expect("mint"));
    storage
        .commit(
            &[StorageWrite::Entry {
                value: EntryRecord::new(entry_id, ROOT_CONVERSATION_ID, "crash-entry"),
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("entry commit");
    std::fs::write(marker, entry_id.get().to_string()).expect("write marker");
    // Keep the process alive until the parent kills it; do not close the
    // storage so the crash happens with the connection open.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

/// A task that commits one running checkpoint, writes the ready marker and
/// blocks until the parent kills the process. A later process completes it.
struct CrashTask(String);

#[async_trait::async_trait]
impl TaskDefinition for CrashTask {
    fn name(&self) -> &str {
        "test.crash"
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
        task: RunningTask,
        runtime: &dyn TaskRuntime,
        context: &rpi_durable::chord::context::Context,
    ) -> SessionResult<()> {
        let count = task
            .checkpoint()
            .and_then(|checkpoint| checkpoint.get("count"))
            .and_then(rpi_durable::chord::json::JsonValue::as_u64)
            .unwrap_or(0);
        if count >= 1 {
            return runtime
                .commit(
                    std::sync::Arc::new(|_tx, _current| {
                        Box::pin(async move {
                            Ok(Some(NextTaskState::Terminal {
                                outcome: rpi_durable::types::TaskOutcome::Completed {
                                    result: serde_json::json!({ "recovered": true }),
                                },
                            }))
                        })
                    }),
                    context,
                )
                .await;
        }
        runtime
            .commit(
                std::sync::Arc::new(|_tx, _current| {
                    Box::pin(async move {
                        Ok(Some(NextTaskState::Running {
                            checkpoint: serde_json::json!({ "phase": "step", "count": 1 }),
                        }))
                    })
                }),
                context,
            )
            .await?;
        std::fs::write(&self.0, "running").expect("write marker");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }
}

async fn harness_then_sleep(path: &str, marker: &str) {
    let storage: std::sync::Arc<dyn rpi_durable::types::Storage> = std::sync::Arc::new(
        open_node_sqlite_storage(path, NodeSqliteStorageOptions::default())
            .await
            .expect("open storage"),
    );
    let registry = std::sync::Arc::new(RegistryImpl::with_builtins(builtin_tasks()));
    registry
        .install(Extension {
            name: "crash".to_string(),
            tasks: vec![std::sync::Arc::new(CrashTask(marker.to_string())) as AnyTask],
            ..Extension::default()
        })
        .expect("install crash task");
    let options = HarnessOptions {
        telemetry: None,
        models: rpi_ai::models::Models::new(None).into(),
        identity: None,
        registry: std::sync::Arc::clone(&registry)
            as std::sync::Arc<dyn rpi_durable::harness::RegistryReader>,
        settings: None,
        env: None,
        conversation_created: None,
        now: None,
        set_timer: None,
        on_wake: None,
        on_report: None,
        on_report_info: None,
    };
    let harness = open_harness(
        std::sync::Arc::clone(&storage),
        options,
        &BACKGROUND_CONTEXT,
    )
    .await
    .expect("open harness");
    let conversation = harness
        .root(&BACKGROUND_CONTEXT, Default::default())
        .await
        .expect("root")
        .id();
    harness
        .session()
        .commit(
            commit_fn(move |tx| {
                Box::pin(async move {
                    tx.create_task(TaskCreate {
                        kind: "test.crash".to_string(),
                        version: 1,
                        input: rpi_durable::chord::json::JsonValue::Null,
                        checkpoint: serde_json::json!({ "phase": "step", "count": 0 }),
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
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("create task");
    harness.resume().expect("resume");
    // The phase writes the marker after its checkpoint commit and then blocks.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

async fn migrate_then_sleep(path: &str, marker: &str) {
    let database = open_node_sqlite_database(path, SqliteDatabaseOptions::default())
        .await
        .expect("open database");
    let mut migrations = sqlite_migrations();
    let v1 = migrations.remove(0);
    let v2 = migrations.remove(0);
    apply_sqlite_migrations(&database, std::slice::from_ref(&v1))
        .await
        .expect("apply v1");
    let task = serde_json::json!({
        "id": 5,
        "conversationId": 1,
        "kind": "test.task",
        "version": 1,
        "input": null,
        "background": false,
        "abortRequested": false,
        "status": "waiting",
        "checkpoint": { "phase": "run" },
        "on": [],
        "policy": "allSettled"
    });
    database
        .run(
            "INSERT INTO tasks (id, conversation_id, kind, status, abort_requested, background, record) \
             VALUES (5, 1, 'test.task', 'waiting', 0, 0, ?)",
            vec![SqliteValue::Text(task.to_string())],
        )
        .await
        .expect("insert task");
    std::fs::write(marker, "migrating").expect("write marker");
    // The recursive CTE makes the migration take long enough for the parent's
    // SIGKILL to land inside the version 2 transaction.
    let mut slow = v2.clone();
    slow.statements.push(
        "WITH RECURSIVE counter(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM counter WHERE value < 100000000) SELECT count(*) FROM counter"
            .to_string(),
    );
    let outcome: Result<(), _> = apply_sqlite_migrations(&database, &[v1, slow]).await;
    let _ = outcome;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
