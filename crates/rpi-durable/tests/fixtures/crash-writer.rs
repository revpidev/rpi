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
use rpi_durable::storage::{
    SqliteDatabaseOptions, SqliteValue, apply_sqlite_migrations, open_node_sqlite_database,
    open_node_sqlite_storage, sqlite_migrations,
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
    let storage = open_node_sqlite_storage(&path, SqliteDatabaseOptions::default())
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
