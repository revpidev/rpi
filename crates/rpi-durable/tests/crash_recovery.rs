//! Real SIGKILL recovery for V18-06 FR-E R4.
//!
//! A child process commits a running checkpoint, writes a ready marker, then
//! blocks until killed. Reopening the same SQLite database counts one restart,
//! rewinds the record to pending, and completes the next phase.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rpi_durable::chord::context::BACKGROUND_CONTEXT;
use rpi_durable::chord::json::JsonValue;
use rpi_durable::harness::{
    AnyTask, Extension, HarnessOptions, NextTaskState, Registry, RegistryImpl, RunningTask,
    TaskDefinition, TaskRuntime, builtin_tasks, open_harness,
};
use rpi_durable::session::SessionResult;
use rpi_durable::storage::{NodeSqliteStorageOptions, open_node_sqlite_storage};
use rpi_durable::types::{Storage, TaskState, TaskStatus};

struct CrashTask;

#[async_trait]
impl TaskDefinition for CrashTask {
    fn name(&self) -> &str {
        "test.crash"
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> JsonValue {
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
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let next = if count >= 1 {
            NextTaskState::Terminal {
                outcome: rpi_durable::types::TaskOutcome::Completed {
                    result: serde_json::json!({ "recovered": true }),
                },
            }
        } else {
            NextTaskState::Running {
                checkpoint: serde_json::json!({ "phase": "step", "count": count + 1 }),
            }
        };
        runtime
            .commit(
                Arc::new(move |_tx, _current| {
                    let next = next.clone();
                    Box::pin(async move { Ok(Some(next)) })
                }),
                context,
            )
            .await
    }
}

#[tokio::test]
async fn a_sigkill_mid_phase_recovers_with_one_restart() {
    let dir = std::env::temp_dir().join(format!(
        "rpi-durable-crash-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    let database = dir.join("durable.db");
    let marker = dir.join("ready");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rpi-durable-crash-writer"))
        .arg(database.to_string_lossy().as_ref())
        .arg(marker.to_string_lossy().as_ref())
        .arg("harness")
        .spawn()
        .expect("spawn crash writer");
    let mut ready = false;
    for _ in 0..400 {
        if marker.exists() {
            ready = true;
            break;
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("crash writer exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(ready, "crash writer did not reach its checkpoint");
    child.kill().expect("kill");
    let _ = child.wait();

    let storage: Arc<dyn Storage> = Arc::new(
        open_node_sqlite_storage(
            database.to_string_lossy().as_ref(),
            NodeSqliteStorageOptions::default(),
        )
        .await
        .expect("open storage"),
    );
    let registry = Arc::new(RegistryImpl::with_builtins(builtin_tasks()));
    registry
        .install(Extension {
            name: "crash".to_string(),
            tasks: vec![Arc::new(CrashTask) as AnyTask],
            ..Extension::default()
        })
        .expect("install");
    let options = HarnessOptions {
        telemetry: None,
        models: rpi_ai::models::Models::new(None).into(),
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
    let harness = open_harness(Arc::clone(&storage), options, &BACKGROUND_CONTEXT)
        .await
        .expect("reopen");
    let inspection = harness.inspect(&BACKGROUND_CONTEXT).await.expect("inspect");
    let task = inspection
        .tasks
        .iter()
        .find(|task| task.record.kind == "test.crash")
        .expect("recovered task");
    assert_eq!(task.record.status(), TaskStatus::Pending);
    assert_eq!(task.record.restarts, Some(1));
    assert!(matches!(task.record.state, TaskState::Pending { .. }));
    let id = task.record.id;

    harness.resume().expect("resume");
    let settled = harness
        .wait_for_task(id, &BACKGROUND_CONTEXT)
        .await
        .expect("settle");
    assert!(matches!(settled.record.state, TaskState::Terminal { .. }));
    assert_eq!(settled.result["recovered"], serde_json::json!(true));
    harness.close(&BACKGROUND_CONTEXT).await.expect("close");
    let _ = std::fs::remove_dir_all(dir);
}
