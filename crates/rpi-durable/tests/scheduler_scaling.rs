mod scheduler_support;
mod session_support;

use std::sync::Arc;

use rpi_durable::session::{TaskCreate, TaskOwnership, commit_fn};
use rpi_durable::types::Storage;
use scheduler_support::{Spec, add_family, all_tasks, context, open_with_storage, root};

#[tokio::test]
async fn scheduling_2048_blocked_records_does_not_rescan_the_task_table() {
    let inner = Arc::new(session_support::ControlledStorage::new());
    let storage: Arc<dyn Storage> = Arc::clone(&inner) as Arc<dyn Storage>;
    let opened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let conversation = root(&opened.harness).await;
    let target = opened
        .harness
        .session()
        .commit(
            commit_fn(move |tx| {
                Box::pin(async move {
                    for _ in 0..2048 {
                        tx.create_task(TaskCreate {
                            kind: "test.missing".to_string(),
                            version: 1,
                            input: serde_json::json!({}),
                            checkpoint: serde_json::json!({ "phase": "step" }),
                            ownership: TaskOwnership::Conversation(conversation),
                            conversation_id: Some(conversation),
                            background: false,
                            abandon_on_restart: false,
                            trace: Default::default(),
                            links: Vec::new(),
                        })
                        .await?;
                    }
                    tx.create_task(TaskCreate {
                        kind: "test.count".to_string(),
                        version: 1,
                        input: serde_json::json!({ "phases": 1 }),
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
            &context(),
        )
        .await
        .expect("seed 2048 tasks");

    let reopened = open_with_storage(Arc::clone(&storage), all_tasks()).await;
    let baseline = inner
        .task_scan_count
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(baseline >= 8, "the initial load must be paginated");
    reopened
        .harness
        .wait_for_task(target, &context())
        .await
        .expect("target completes");
    assert_eq!(
        inner
            .task_scan_count
            .load(std::sync::atomic::Ordering::SeqCst),
        baseline,
        "later planning and finalize passes must use their in-memory candidates"
    );
    reopened.harness.close(&context()).await.expect("close");
}

#[tokio::test]
async fn parallel_waiters_and_multiple_holders_reach_terminal() {
    let opened = open_with_storage(
        Arc::new(rpi_durable::storage::MemoryStorage::new()),
        all_tasks(),
    )
    .await;
    let conversation = root(&opened.harness).await;
    let mut parents = Vec::new();
    for _ in 0..2 {
        let children = (0..16)
            .map(|_| {
                Spec::child(
                    "test.count",
                    serde_json::json!({ "phases": 1 }),
                    rpi_durable::ids::id_from_number(0),
                )
            })
            .collect::<Vec<_>>();
        let (parent, children) = add_family(
            &opened.harness,
            conversation,
            Spec::owned("test.count", serde_json::json!({ "phases": 1 })),
            children,
        )
        .await;
        let waiter = scheduler_support::add_task(
            &opened.harness,
            conversation,
            Spec::owned(
                "test.wait",
                serde_json::json!({ "on": children.iter().map(|id| id.get()).collect::<Vec<_>>() }),
            ),
        )
        .await;
        parents.push((parent, waiter));
    }
    for (parent, waiter) in parents {
        for id in [parent, waiter] {
            let settled = opened
                .harness
                .wait_for_task(id, &context())
                .await
                .expect("terminal");
            assert!(matches!(
                settled.record.state,
                rpi_durable::types::TaskState::Terminal { .. }
            ));
        }
    }
    opened.harness.close(&context()).await.expect("close");
}
