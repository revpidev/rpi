//! Read-only Session, Harness, and JSONL storage behavior.

mod test_support;

use std::sync::Arc;

use rpi_durable::chord::context::BACKGROUND_CONTEXT;
use rpi_durable::errors::StorageError;
use rpi_durable::fs::{FileSystem, StdFileSystem};
use rpi_durable::harness::{Harness, HarnessOptions, RegistryImpl, builtin_tasks, open_harness};
use rpi_durable::session::{SessionError, SessionImpl, commit_fn};
use rpi_durable::storage::{JsonlStorage, JsonlStorageOptions, MemoryStorage};
use rpi_durable::types::{ConversationRecord, Storage, StorageExt, TaskRecord, TaskState};
use test_support::TempDir;

fn running_task() -> TaskRecord {
    TaskRecord {
        id: rpi_durable::ids::id_from_number(5),
        conversation_id: rpi_durable::ROOT_CONVERSATION_ID,
        kind: "test.running".to_string(),
        version: 1,
        input: serde_json::Value::Null,
        trace: None,
        links: Vec::new(),
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: false,
        restarts: None,
        started_at: Some(1.0),
        ended_at: None,
        state: TaskState::Running {
            checkpoint: serde_json::json!({"phase": "work"}),
        },
        memos: None,
    }
}

#[tokio::test]
async fn session_read_only_rejects_without_failing() {
    let storage = Arc::new(MemoryStorage::new());
    let session = SessionImpl::new_with_options(storage, None, true);
    let error = session
        .commit(
            commit_fn(|_tx| Box::pin(async move { Ok(()) })),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect_err("read-only commit");
    assert!(matches!(error, SessionError::ReadOnlySession));
    assert!(session.failure().is_none());
}

#[tokio::test]
async fn harness_read_only_does_not_recover_or_resume() {
    let storage = Arc::new(MemoryStorage::new());
    storage
        .commit_one(
            &[
                rpi_durable::types::StorageWrite::Conversation {
                    value: ConversationRecord {
                        id: rpi_durable::ROOT_CONVERSATION_ID,
                        parent: None,
                        owner: None,
                    },
                },
                rpi_durable::types::StorageWrite::Task {
                    value: running_task(),
                },
            ],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("seed");
    let registry = Arc::new(RegistryImpl::with_builtins(builtin_tasks()));
    let harness = open_harness(
        storage,
        HarnessOptions {
            telemetry: None,
            models: rpi_ai::models::Models::new(None).into(),
            identity: None,
            registry,
            settings: None,
            env: None,
            conversation_created: None,
            now: None,
            set_timer: None,
            on_wake: None,
            on_report: None,
            read_only: true,
        },
        &BACKGROUND_CONTEXT,
    )
    .await
    .expect("read-only harness");
    let task = harness
        .get_task(rpi_durable::ids::id_from_number(5), &BACKGROUND_CONTEXT)
        .await
        .expect("task")
        .expect("record");
    assert!(
        matches!(task.state, TaskState::Running { .. }),
        "running is not recovered"
    );
    assert!(matches!(
        harness.resume(),
        Err(SessionError::ReadOnlySession)
    ));
    assert!(!harness.wake().active);
}

#[tokio::test]
async fn jsonl_read_only_preserves_torn_and_unconfirmed_bytes() {
    let temp = TempDir::new("jsonl-read-only");
    let fs: Arc<dyn FileSystem> = Arc::new(StdFileSystem::new(temp.path()));
    let directory = temp.path_str();
    let writer = JsonlStorage::open(
        &directory,
        Arc::clone(&fs),
        JsonlStorageOptions {
            fsync: false,
            read_only: false,
        },
        &BACKGROUND_CONTEXT,
    )
    .await
    .expect("writer");
    writer
        .commit_one(
            &[rpi_durable::types::StorageWrite::Conversation {
                value: ConversationRecord {
                    id: rpi_durable::ROOT_CONVERSATION_ID,
                    parent: None,
                    owner: None,
                },
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("root");
    let main = temp.path().join("main.jsonl");
    std::fs::write(&main, b"{\"torn\":").expect("torn main");
    let reader = JsonlStorage::open(
        &directory,
        Arc::clone(&fs),
        JsonlStorageOptions {
            fsync: false,
            read_only: true,
        },
        &BACKGROUND_CONTEXT,
    )
    .await
    .expect("read-only replay");
    assert_eq!(std::fs::read(&main).expect("main bytes"), b"{\"torn\":");
    assert!(matches!(
        reader
            .commit(&[], &mut |_, _| {}, &BACKGROUND_CONTEXT)
            .await,
        Err(StorageError::JsonlStorageReadOnlyError)
    ));
    assert!(matches!(
        reader.mint_id().await,
        Err(StorageError::JsonlStorageReadOnlyError)
    ));

    let writer = JsonlStorage::open(
        &directory,
        fs,
        JsonlStorageOptions {
            fsync: false,
            read_only: false,
        },
        &BACKGROUND_CONTEXT,
    )
    .await
    .expect("concurrent writer");
    writer
        .commit_one(
            &[rpi_durable::types::StorageWrite::Conversation {
                value: ConversationRecord {
                    id: rpi_durable::ROOT_CONVERSATION_ID,
                    parent: None,
                    owner: None,
                },
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("concurrent write");
    assert!(
        std::fs::read_to_string(&main)
            .expect("rewritten main")
            .contains("commit")
    );
}

#[tokio::test]
async fn jsonl_read_only_does_not_truncate_an_unconfirmed_sidecar_tail() {
    let temp = TempDir::new("jsonl-read-only-unconfirmed");
    let fs: Arc<dyn FileSystem> = Arc::new(StdFileSystem::new(temp.path()));
    let directory = temp.path_str();
    let writer = JsonlStorage::open(
        &directory,
        Arc::clone(&fs),
        JsonlStorageOptions {
            fsync: false,
            read_only: false,
        },
        &BACKGROUND_CONTEXT,
    )
    .await
    .expect("writer");
    writer
        .commit_one(
            &[
                rpi_durable::types::StorageWrite::Conversation {
                    value: ConversationRecord {
                        id: rpi_durable::ROOT_CONVERSATION_ID,
                        parent: None,
                        owner: None,
                    },
                },
                rpi_durable::types::StorageWrite::Task {
                    value: running_task(),
                },
            ],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("confirmed task");
    let sidecar = temp.path().join("task-5.jsonl");
    let confirmed = std::fs::read_to_string(&sidecar).expect("confirmed sidecar");
    let unconfirmed = format!(
        "{{\"format\":1,\"type\":\"record\",\"seq\":999,\"ordinal\":0,\"payload\":{{\"type\":\"task\",\"value\":{}}}}}\n",
        serde_json::to_string(&running_task()).expect("task json")
    );
    std::fs::write(&sidecar, format!("{confirmed}{unconfirmed}")).expect("unconfirmed tail");
    let before = std::fs::read(&sidecar).expect("bytes before");
    let modified = std::fs::metadata(&sidecar)
        .and_then(|metadata| metadata.modified())
        .expect("mtime before");
    let directory_modified = std::fs::metadata(temp.path())
        .and_then(|metadata| metadata.modified())
        .expect("directory mtime before");
    let reader = JsonlStorage::open(
        &directory,
        Arc::clone(&fs),
        JsonlStorageOptions {
            fsync: false,
            read_only: true,
        },
        &BACKGROUND_CONTEXT,
    )
    .await
    .expect("read-only replay");
    assert!(
        reader
            .task(rpi_durable::ids::id_from_number(5), &BACKGROUND_CONTEXT)
            .await
            .expect("task")
            .is_some()
    );
    assert_eq!(std::fs::read(&sidecar).expect("bytes after"), before);
    assert_eq!(
        std::fs::metadata(&sidecar)
            .and_then(|metadata| metadata.modified())
            .expect("mtime after"),
        modified
    );
    assert_eq!(
        std::fs::metadata(temp.path())
            .and_then(|metadata| metadata.modified())
            .expect("directory mtime after"),
        directory_modified
    );
}
