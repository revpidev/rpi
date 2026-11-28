mod generation_support;
mod test_support;

use std::sync::Arc;

use generation_support::{context, test_models};
use rpi_durable::fs::{FileSystem, StdFileSystem};
use rpi_durable::harness::{BlockedReason, TaskInspectionState};
use rpi_durable::ids::{EntryId, TaskId, id_from_number};
use rpi_durable::storage::{
    JsonlStorage, JsonlStorageOptions, MemoryStorage, NodeSqliteStorageOptions,
    open_node_sqlite_storage,
};
use rpi_durable::types::{
    ConversationRecord, EntryRecord, Storage, StorageExt, StorageWrite, TaskRecord, TaskState,
};
use test_support::TempDir;

fn task(id: TaskId, kind: &str, version: u64, input: serde_json::Value) -> TaskRecord {
    TaskRecord {
        id,
        conversation_id: rpi_durable::ROOT_CONVERSATION_ID,
        kind: kind.to_string(),
        version,
        input,
        trace: None,
        links: Vec::new(),
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: false,
        restarts: None,
        started_at: None,
        ended_at: None,
        state: TaskState::Pending {
            checkpoint: serde_json::json!({ "phase": "call" }),
        },
        memos: None,
    }
}

fn assistant(id: EntryId) -> EntryRecord {
    let mut entry = EntryRecord::new(id, rpi_durable::ROOT_CONVERSATION_ID, "pi.assistant");
    entry.model = Some(vec![rpi_ai::types::Message::Assistant(
        generation_support::tool_use_message("missing", "c1", serde_json::json!({})),
    )]);
    entry
}

async fn open_backend(kind: &str, temp: &TempDir) -> Arc<dyn Storage> {
    match kind {
        "memory" => Arc::new(MemoryStorage::new()),
        "jsonl" => {
            let fs: Arc<dyn FileSystem> = Arc::new(StdFileSystem::new(temp.path()));
            Arc::new(
                JsonlStorage::open(
                    &temp.path_str(),
                    fs,
                    JsonlStorageOptions {
                        fsync: false,
                        read_only: false,
                    },
                    &context(),
                )
                .await
                .expect("jsonl"),
            )
        }
        "sqlite" => Arc::new(
            open_node_sqlite_storage(
                &temp.path().join("durable.db").to_string_lossy(),
                NodeSqliteStorageOptions::default(),
            )
            .await
            .expect("sqlite"),
        ),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn old_tool_and_generation_records_migrate_on_jsonl_and_sqlite() {
    for backend in ["memory", "jsonl", "sqlite"] {
        let temp = TempDir::new("task-migrations");
        let storage = open_backend(backend, &temp).await;
        storage
            .commit_one(
                &[
                    StorageWrite::Conversation {
                        value: ConversationRecord {
                            id: rpi_durable::ROOT_CONVERSATION_ID,
                            parent: None,
                            owner: None,
                        },
                    },
                    StorageWrite::Entry {
                        value: assistant(id_from_number(2)),
                    },
                    StorageWrite::Task {
                        value: task(
                            id_from_number(3),
                            "pi.tool",
                            1,
                            serde_json::json!({ "assistant": 2, "callId": "c1" }),
                        ),
                    },
                    StorageWrite::Task {
                        value: TaskRecord {
                            state: TaskState::Pending {
                                checkpoint: serde_json::json!({ "phase": "prepare", "attempt": 1 }),
                            },
                            ..task(id_from_number(4), "pi.generation", 1, serde_json::json!({}))
                        },
                    },
                    StorageWrite::Task {
                        value: task(
                            id_from_number(5),
                            "pi.tool",
                            2,
                            serde_json::json!({
                                "kind": "model",
                                "assistant": 2,
                                "callId": "c1"
                            }),
                        ),
                    },
                ],
                &context(),
            )
            .await
            .expect("old fixture");
        let opened = generation_support::open_with_storage(
            Arc::clone(&storage),
            Vec::new(),
            None,
            test_models(Vec::new()),
        )
        .await;
        for id in [id_from_number(3), id_from_number(5)] {
            let settled = opened
                .harness
                .wait_for_task(id, &context())
                .await
                .expect("tool migrates");
            assert_eq!(settled.record.version, 3, "{backend}");
            assert_eq!(settled.result["kind"], serde_json::json!("model"));
            assert_eq!(settled.record.input["kind"], serde_json::json!("model"));
        }
        let generation = opened
            .harness
            .wait_for_task(id_from_number(4), &context())
            .await
            .expect("generation migrates");
        assert_eq!(generation.record.version, 2, "{backend}");
        assert!(
            matches!(
                generation.record.state,
                TaskState::Terminal {
                    outcome: rpi_durable::types::TaskOutcome::Failed { .. }
                }
            ),
            "{backend}: {generation:?}"
        );
        opened.harness.close(&context()).await.expect("close");
    }
}

#[tokio::test]
async fn an_old_definition_rejects_a_newer_task_record() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    storage
        .commit_one(
            &[
                StorageWrite::Conversation {
                    value: ConversationRecord {
                        id: rpi_durable::ROOT_CONVERSATION_ID,
                        parent: None,
                        owner: None,
                    },
                },
                StorageWrite::Task {
                    value: task(
                        id_from_number(3),
                        "pi.tool",
                        4,
                        serde_json::json!({
                            "kind": "model",
                            "assistant": 2,
                            "callId": "c1",
                            "unoffered": true
                        }),
                    ),
                },
            ],
            &context(),
        )
        .await
        .expect("new fixture");
    let opened = generation_support::open_with_storage(
        Arc::clone(&storage),
        Vec::new(),
        None,
        test_models(Vec::new()),
    )
    .await;
    let inspection = opened.harness.inspect(&context()).await.expect("inspect");
    let task = inspection
        .tasks
        .iter()
        .find(|task| task.record.id == TaskId::from_number(3))
        .expect("task");
    assert!(matches!(
        task.state,
        TaskInspectionState::Blocked {
            reason: BlockedReason::TaskTooOld,
            ..
        }
    ));
    assert_eq!(task.record.version, 4);
    opened.harness.close(&context()).await.expect("close");
}
