mod session_support;

use std::sync::Arc;

use rpi_durable::chord::context::BACKGROUND_CONTEXT;
use rpi_durable::ids::{ConversationId, TaskId, id_from_number};
use rpi_durable::session::{CachedStorage, CommitChange};
use rpi_durable::types::{
    ConversationRecord, Storage, StorageExt, StorageWrite, TaskRecord, TaskState,
};
use session_support::ControlledStorage;

fn conversation(id: ConversationId) -> ConversationRecord {
    ConversationRecord {
        id,
        parent: None,
        owner: None,
    }
}

fn task(id: TaskId, conversation_id: ConversationId, state: TaskState) -> TaskRecord {
    TaskRecord {
        id,
        conversation_id,
        kind: "test.cached".to_string(),
        version: 1,
        input: serde_json::json!({}),
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
        state,
        memos: None,
    }
}

fn cached() -> (Arc<CachedStorage>, Arc<ControlledStorage>) {
    let inner = Arc::new(ControlledStorage::new());
    let cached = Arc::new(CachedStorage::new(Arc::clone(&inner) as Arc<dyn Storage>));
    (cached, inner)
}

#[tokio::test]
async fn publication_shares_arc_records_and_keeps_awaited_terminal_tasks() {
    let (cache, inner) = cached();
    let conversation_id = id_from_number(1);
    let task_id = id_from_number(2);
    let root = conversation(conversation_id);
    let pending = task(
        task_id,
        conversation_id,
        TaskState::Pending {
            checkpoint: serde_json::json!({ "phase": "prepare" }),
        },
    );
    cache
        .commit_one(
            &[
                StorageWrite::Conversation {
                    value: root.clone(),
                },
                StorageWrite::Task {
                    value: pending.clone(),
                },
            ],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("store");
    let read = cache
        .conversation(conversation_id, &BACKGROUND_CONTEXT)
        .await
        .expect("read")
        .expect("conversation");
    assert_eq!(
        inner
            .conversation_read_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    cache.published(&[
        CommitChange::Table(StorageWrite::Conversation { value: root }),
        CommitChange::Table(StorageWrite::Task {
            value: pending.clone(),
        }),
    ]);
    let published = cache
        .conversation(conversation_id, &BACKGROUND_CONTEXT)
        .await
        .expect("read")
        .expect("conversation");
    let again = cache
        .conversation(conversation_id, &BACKGROUND_CONTEXT)
        .await
        .expect("read")
        .expect("conversation");
    assert_ne!(Arc::as_ptr(&read), Arc::as_ptr(&published));
    assert!(Arc::ptr_eq(&published, &again));
    assert_eq!(
        inner
            .conversation_read_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(Arc::get_mut(&mut again.clone()).is_none());

    let live = cache
        .task(task_id, &BACKGROUND_CONTEXT)
        .await
        .expect("task")
        .expect("live");
    assert_eq!(live.state, pending.state);
    assert_eq!(
        inner
            .task_read_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let terminal = task(
        task_id,
        conversation_id,
        TaskState::Terminal {
            outcome: rpi_durable::types::TaskOutcome::Completed {
                result: serde_json::json!({ "done": true }),
            },
        },
    );
    cache.published(&[CommitChange::Table(StorageWrite::Task {
        value: terminal.clone(),
    })]);
    let waiter = task(
        id_from_number(3),
        conversation_id,
        TaskState::Waiting {
            checkpoint: serde_json::json!({ "phase": "join" }),
            on: vec![task_id],
            policy: rpi_durable::types::JoinPolicy::AllSettled,
        },
    );
    cache.published(&[CommitChange::Table(StorageWrite::Task { value: waiter })]);
    let sizes = cache.sizes();
    assert_eq!(sizes.live_tasks, 1);
    assert_eq!(sizes.awaited, 1);
    assert_eq!(sizes.reads_in_flight, 0);
    let held = cache
        .task(task_id, &BACKGROUND_CONTEXT)
        .await
        .expect("outcome")
        .expect("terminal");
    assert_eq!(held.state, terminal.state);
    assert_eq!(
        inner
            .task_read_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn a_read_that_started_before_publication_cannot_overwrite_it() {
    let (cache, inner) = cached();
    let id = id_from_number(10);
    let before = conversation(id);
    cache
        .commit_one(
            &[StorageWrite::Conversation {
                value: before.clone(),
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("store");
    let gate = inner.hold_conversation();
    let reading = {
        let cache = Arc::clone(&cache);
        tokio::spawn(async move { cache.conversation(id, &BACKGROUND_CONTEXT).await })
    };
    gate.entered().await;
    let mut newer = before.clone();
    newer.owner = Some(rpi_durable::types::ConversationOwner {
        conversation_id: id,
        task_id: id_from_number(99),
    });
    cache.published(&[CommitChange::Table(StorageWrite::Conversation {
        value: newer.clone(),
    })]);
    gate.release();
    let old = reading
        .await
        .expect("join")
        .expect("read")
        .expect("old result");
    assert_eq!(old.as_ref(), &before);
    let current = cache
        .conversation(id, &BACKGROUND_CONTEXT)
        .await
        .expect("current")
        .expect("published");
    assert_eq!(current.as_ref(), &newer);
    assert_eq!(
        inner
            .conversation_read_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(cache.sizes().reads_in_flight, 0);
}
