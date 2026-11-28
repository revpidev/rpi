//! LIVE_DOC v1/v2 to v3 migration fixtures for every storage backend.

mod test_support;

use std::sync::{Arc, LazyLock};

use rpi_durable::chord::context::BACKGROUND_CONTEXT;
use rpi_durable::documents::{DocDefinition, DocScope, DocToken, define_doc};
use rpi_durable::fs::{FileSystem, StdFileSystem};
use rpi_durable::harness::LIVE_DOC;
use rpi_durable::session::{DocOwner, commit_fn, create_session};
use rpi_durable::storage::{
    JsonlStorage, JsonlStorageOptions, MemoryStorage, NodeSqliteStorageOptions,
    open_node_sqlite_storage,
};
use rpi_durable::types::{
    ConversationRecord, DocumentContent, DocumentCreate, DocumentFork, DocumentHistory,
    DocumentPoint, DocumentScope, Storage, StorageExt, StorageWrite,
};
use test_support::TempDir;

fn old_reader() -> &'static DocToken {
    static TOKEN: LazyLock<DocToken> = LazyLock::new(|| {
        define_doc(DocDefinition {
            kind: "pi.live".to_string(),
            version: 1,
            scope: DocScope::Conversation,
            history: Some(DocumentHistory::Latest),
            fork: Some(DocumentFork::Initial),
            family: false,
            initial: Arc::new(|_| Default::default()),
            migrate: None,
            checkpoint_when: Arc::new(|_, _, _| false),
        })
    });
    &TOKEN
}

async fn seed(storage: &Arc<dyn Storage>, version: u64) {
    storage
        .commit_one(
            &[StorageWrite::Conversation {
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
    let mut value = serde_json::json!({
        "run": {
            "taskId": 3,
            "inputs": [],
            "turns": (version == 3).then_some(7),
        },
        "compactions": [{
            "taskId": 4,
            "reason": (version == 1).then_some("threshold"),
            "trigger": (version != 1).then_some("threshold"),
        }],
        "compactionFailure": {
            "taskId": 5,
            "reason": (version == 1).then_some("overflow"),
            "trigger": (version != 1).then_some("overflow"),
            "error": "failed",
        },
    });
    if version != 1 {
        value["compactions"][0]
            .as_object_mut()
            .expect("compaction")
            .remove("reason");
        value["compactionFailure"]
            .as_object_mut()
            .expect("failure")
            .remove("reason");
    }
    storage
        .commit_one(
            &[StorageWrite::DocumentCreate {
                record: DocumentCreate {
                    id: rpi_durable::ids::id_from_number(10),
                    kind: "pi.live".to_string(),
                    scope: DocumentScope::Conversation {
                        conversation_id: rpi_durable::ROOT_CONVERSATION_ID,
                    },
                    history: Some(DocumentHistory::Latest),
                    fork: Some(DocumentFork::Initial),
                    key: None,
                },
                content: DocumentContent::Base {
                    version,
                    value: value.as_object().cloned().expect("object"),
                },
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("old LIVE_DOC");
}

async fn verify(storage: Arc<dyn Storage>, version: u64) {
    seed(&storage, version).await;
    let session = create_session(Arc::clone(&storage), None);
    let first = session
        .snapshot(
            &LIVE_DOC,
            DocOwner::Conversation(rpi_durable::ROOT_CONVERSATION_ID),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("migrated snapshot")
        .expect("document");
    assert_eq!(first["run"]["turns"], if version == 3 { 7 } else { 0 });
    assert_eq!(first["compactions"][0]["trigger"], "threshold");
    assert!(first["compactions"][0].get("reason").is_none());
    assert_eq!(
        first["compactionFailure"]["trigger"], "overflow",
        "version {version}: {first:?}"
    );
    assert_eq!(first["compactionFailure"]["error"], "failed");
    let second = create_session(Arc::clone(&storage), None)
        .snapshot(
            &LIVE_DOC,
            DocOwner::Conversation(rpi_durable::ROOT_CONVERSATION_ID),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("idempotent snapshot")
        .expect("document");
    assert_eq!(first, second);

    session
        .commit(
            commit_fn(|tx| {
                Box::pin(async move {
                    tx.doc(
                        &LIVE_DOC,
                        DocOwner::Conversation(rpi_durable::ROOT_CONVERSATION_ID),
                        None,
                    )
                    .await?
                    .write()?["next"] = serde_json::json!(true);
                    Ok(())
                })
            }),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("next write");
    let stored = storage
        .document(
            rpi_durable::ids::id_from_number(10),
            DocumentPoint::CURRENT,
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("stored")
        .expect("document");
    assert_eq!(stored.version, 3);
    assert!(
        create_session(Arc::clone(&storage), None)
            .snapshot(
                old_reader(),
                DocOwner::Conversation(rpi_durable::ROOT_CONVERSATION_ID),
                &BACKGROUND_CONTEXT
            )
            .await
            .is_err(),
        "old reader rejects the upgraded document"
    );
}

#[tokio::test]
async fn live_versions_migrate_in_memory() {
    for version in 1..=3 {
        verify(Arc::new(MemoryStorage::new()), version).await;
    }
}

#[tokio::test]
async fn live_versions_migrate_in_jsonl() {
    for version in 1..=3 {
        let temp = TempDir::new("live-jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(StdFileSystem::new(temp.path()));
        let storage = JsonlStorage::open(
            &temp.path_str(),
            fs,
            JsonlStorageOptions {
                fsync: false,
                read_only: false,
            },
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("jsonl");
        verify(Arc::new(storage), version).await;
    }
}

#[tokio::test]
async fn live_versions_migrate_in_sqlite() {
    for version in 1..=3 {
        let temp = TempDir::new("live-sqlite");
        let storage = open_node_sqlite_storage(
            &temp.path().join("live.sqlite").to_string_lossy(),
            NodeSqliteStorageOptions::default(),
        )
        .await
        .expect("sqlite");
        verify(Arc::new(storage), version).await;
    }
}
