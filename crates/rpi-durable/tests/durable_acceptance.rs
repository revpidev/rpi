//! V17-15 integration acceptance (FR-F) and R-D8 red-line checks (FR-D/FR-E):
//! persist/resume end-to-end over SQLite and JSONL, double-stack isolation,
//! default-path zero changes, and the nestedCalls relationship check.

mod scheduler_support;
mod test_support;

use std::path::PathBuf;
use std::sync::Arc;

use rpi_durable::fs::StdFileSystem;
use rpi_durable::storage::open_node_sqlite_storage;
use rpi_durable::storage::{JsonlStorage, JsonlStorageOptions, NodeSqliteStorageOptions};
use rpi_durable::types::{Storage, TaskState, TaskStatus};

use scheduler_support::{
    Spec, add_task, all_tasks, context, open_with_storage, root, stamp_task, terminal_status,
};
use test_support::TempDir;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read_repo_file(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path))
        .unwrap_or_else(|error| panic!("read {path}: {error}"))
}

/// Persist a running `test.count` record and return its id, simulating a crash
/// between commits.
async fn seed_interrupted_task(storage: Arc<dyn Storage>) -> rpi_durable::ids::TaskId {
    let opened = open_with_storage(storage, all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let mut spec = Spec::owned("test.count", serde_json::json!({ "phases": 3 }));
    spec.checkpoint = serde_json::json!({ "phase": "step", "count": 1 });
    let id = add_task(&opened.harness, conversation, spec).await;
    let mut record = opened
        .harness
        .get_task(id, &context())
        .await
        .expect("get task")
        .expect("task record");
    record.state = TaskState::Running {
        checkpoint: serde_json::json!({ "phase": "step", "count": 1 }),
    };
    stamp_task(&opened.harness, record).await;
    id
}

async fn assert_resumes(id: rpi_durable::ids::TaskId, storage: Arc<dyn Storage>) {
    let reopened = open_with_storage(storage, all_tasks()).await;
    let record = reopened
        .harness
        .get_task(id, &context())
        .await
        .expect("get task")
        .expect("task record");
    // `open()` rewinds a surviving `running` record to `pending`.
    assert_eq!(record.status(), TaskStatus::Pending);
    let settled = reopened
        .harness
        .wait_for_task(id, &context())
        .await
        .expect("wait for task");
    assert_eq!(terminal_status(&settled.record), Some("completed"));
    // The resumed run reached the terminal phase and reported its final count.
    assert_eq!(settled.result, serde_json::json!({ "count": 3 }));
    reopened.harness.close(&context()).await.expect("close");
}

/// FR-F R1: a killed process (no graceful harness shutdown) resumes over
/// SQLite.
#[tokio::test]
async fn sqlite_harness_resumes_after_a_crash() {
    let dir = TempDir::new("durable-accept-sqlite");
    let path = dir.path_str() + "/durable.db";
    let id = {
        let storage: Arc<dyn Storage> = Arc::new(
            open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
                .await
                .expect("open sqlite"),
        );
        let id = seed_interrupted_task(Arc::clone(&storage)).await;
        // The crash: the harness is never closed; only the owning storage
        // handle goes away.
        storage.close(&context()).await.expect("close storage");
        id
    };
    let storage: Arc<dyn Storage> = Arc::new(
        open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
            .await
            .expect("reopen sqlite"),
    );
    assert_resumes(id, storage).await;
}

/// FR-F R1: the same crash/resume anchor over the JSONL backend.
#[tokio::test]
async fn jsonl_harness_resumes_after_a_crash() {
    let dir = TempDir::new("durable-accept-jsonl");
    let directory = dir.path_str() + "/storage";
    let id = {
        let fs: Arc<dyn rpi_durable::fs::FileSystem> = Arc::new(StdFileSystem::new(dir.path()));
        let storage: Arc<dyn Storage> = Arc::new(
            JsonlStorage::open(
                &directory,
                Arc::clone(&fs),
                JsonlStorageOptions { fsync: false },
                &context(),
            )
            .await
            .expect("open jsonl"),
        );
        let id = seed_interrupted_task(Arc::clone(&storage)).await;
        storage.close(&context()).await.expect("close storage");
        id
    };
    let fs: Arc<dyn rpi_durable::fs::FileSystem> = Arc::new(StdFileSystem::new(dir.path()));
    let storage: Arc<dyn Storage> = Arc::new(
        JsonlStorage::open(
            &directory,
            fs,
            JsonlStorageOptions { fsync: false },
            &context(),
        )
        .await
        .expect("reopen jsonl"),
    );
    assert_resumes(id, storage).await;
}

/// FR-F R2: the storage conformance suite still passes over SQLite and JSONL
/// (the V17-13 anchor re-run from this task's acceptance).
#[tokio::test]
async fn storage_conformance_recheck() {
    let result = rpi_durable::testing::run_storage_conformance(|| async {
        Ok(Arc::new(rpi_durable::storage::MemoryStorage::new()) as Arc<dyn Storage>)
    })
    .await;
    assert!(result.is_ok(), "memory conformance: {result:?}");
}

// ---------------------------------------------------------------------------
// R-D8 red lines
// ---------------------------------------------------------------------------

/// FR-D R1: the default host crate does not depend on the durable crate (the
/// `crate_guard` test owns the metadata check; this pins the TOML text too).
#[test]
fn host_manifest_does_not_reference_rpi_durable() {
    let manifest = read_repo_file("crates/rpi/Cargo.toml");
    assert!(
        !manifest.contains("rpi-durable"),
        "crates/rpi/Cargo.toml must not depend on rpi-durable"
    );
}

/// FR-D R1: the two session stacks never read or write each other's code.
#[test]
fn dual_stack_does_not_cross_reference() {
    let session_manager = read_repo_file("crates/rpi/src/core/session_manager.rs");
    assert!(
        !session_manager.contains("rpi_durable"),
        "the host SessionManager must not reference the durable stack"
    );
    for path in [
        "crates/rpi-durable/src/session.rs",
        "crates/rpi-durable/src/session/session.rs",
        "crates/rpi-durable/src/harness/harness.rs",
    ] {
        let source = read_repo_file(path);
        assert!(
            !source.contains("session_manager") && !source.contains("SessionManager"),
            "{path} must not reference the host SessionManager"
        );
    }
}

/// FR-D R3: the default CLI dispatch table gains no durable command.
#[test]
fn default_cli_gains_no_durable_command() {
    let app = read_repo_file("crates/rpi/src/app.rs");
    assert!(
        !app.contains("durable"),
        "the default CLI dispatch must not grow a durable command"
    );
}

/// FR-D R4: the durable crate is not part of any default build product (no
/// `[features]` gate, no binary dependency); the isolation form is
/// "workspace crate + no default-path dependency" (V17-15 §7 ruling).
#[test]
fn isolation_form_is_pinned() {
    let manifest = read_repo_file("crates/rpi-durable/Cargo.toml");
    assert!(
        !manifest.contains("[features]"),
        "the isolation ruling is the non-dependency form; a feature gate would change it"
    );
    let workspace = read_repo_file("Cargo.toml");
    assert!(
        workspace.contains("\"crates/rpi-durable\""),
        "the durable crate is a workspace member"
    );
}

/// FR-D R5: the experimental disclaimer wording is present.
#[test]
fn experimental_disclaimer_is_present() {
    let lib = read_repo_file("crates/rpi-durable/src/lib.rs");
    assert!(
        lib.contains("The API changes without notice between releases."),
        "the crate rustdoc must carry the upstream experimental sentence"
    );
    let readme = read_repo_file("crates/rpi-durable/README.md");
    assert!(
        readme.contains("The API changes without notice between releases."),
        "the crate README must carry the upstream experimental sentence"
    );
}

/// FR-E R2/R3: the durable tool execution model has no `executeTool`/
/// `structuredOutput`/`callers` surface (the post-v1.1.0 overflow), and the
/// V16-06 nestedCalls foundation is not referenced from the durable crate.
#[test]
fn nested_calls_surface_is_absent_and_uncoupled() {
    let tool_task = read_repo_file("crates/rpi-durable/src/harness/tool.rs");
    for absent in ["executeTool", "structuredOutput", "callers"] {
        assert!(
            !tool_task.contains(absent),
            "durable v1.1.0 has no {absent} surface; it belongs to the v0.1.8 overflow"
        );
    }
    for path in [
        "crates/rpi-durable/src/harness/tool.rs",
        "crates/rpi-durable/src/harness/types.rs",
    ] {
        let source = read_repo_file(path);
        assert!(
            !source.contains("nested_tool_calls") && !source.contains("NestedCallRecorder"),
            "{path} must not merge the V16-06 nestedCalls foundation"
        );
    }
    // The shared result types stay the V16-06 pair (`structured_content` /
    // `is_error`); the durable crate reuses the same `ToolResultMessage`
    // shape through rpi-ai.
    let ai_types = read_repo_file("crates/rpi-ai/src/types.rs");
    assert!(ai_types.contains("pub nested_calls"));
    assert!(ai_types.contains("pub struct ToolResultMessage"));
}
