//! SQLite backend tests: shared conformance suite (fresh + across reopen),
//! pragma/reopen anchors, crash survival, migrations and facade semantics.

mod test_support;

use std::sync::Arc;

use rpi_durable::chord::context::BACKGROUND_CONTEXT;
use rpi_durable::errors::StorageError;
use rpi_durable::ids::{DocumentId, EntryId, ROOT_CONVERSATION_ID, id_from_number};
use rpi_durable::storage::{
    CURRENT_SQLITE_SCHEMA_VERSION, DEFAULT_SQLITE_SCHEMA, NodeSqliteStorageOptions, SqliteDatabase,
    SqliteDatabaseOptions, SqliteMigration, SqliteStorage, SqliteValue, apply_sqlite_migrations,
    create_sqlite_migrations, open_node_sqlite_storage, sqlite_migrations,
};
use rpi_durable::testing::run_storage_conformance;
use rpi_durable::types::{
    ConversationRecord, DocumentContent, DocumentCreate, DocumentScope, EntryRecord, JoinPolicy,
    Storage, StorageWrite, TaskRecord, TaskState,
};
use test_support::{ReopeningStorage, TempDir, cleanup_temp_dirs};

async fn open_sqlite(temp: &TempDir) -> SqliteStorage {
    open_node_sqlite_storage(
        &temp.path().join("storage.sqlite").to_string_lossy(),
        NodeSqliteStorageOptions::default(),
    )
    .await
    .expect("open storage")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_storage_conformance() {
    let result = run_storage_conformance(|| async {
        let dir = TempDir::new("rpi-durable-sqlite");
        let storage = open_sqlite(&dir).await;
        Ok(Arc::new(storage) as Arc<dyn Storage>)
    })
    .await;
    cleanup_temp_dirs("rpi-durable-sqlite");
    assert!(
        result.is_ok(),
        "conformance failures:\n{}",
        result.err().unwrap_or_default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_storage_conformance_across_reopen() {
    let result = run_storage_conformance(|| async {
        let dir = TempDir::new("rpi-durable-sqlite-reopen");
        let path = dir
            .path()
            .join("storage.sqlite")
            .to_string_lossy()
            .into_owned();
        let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default()).await?;
        let reopen_path = path.clone();
        let reopened = ReopeningStorage::new(Box::new(storage), move || {
            let path = reopen_path.clone();
            Box::pin(async move {
                let storage =
                    open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default()).await?;
                Ok(Box::new(storage) as Box<dyn Storage>)
            })
        });
        Ok(Arc::new(reopened) as Arc<dyn Storage>)
    })
    .await;
    cleanup_temp_dirs("rpi-durable-sqlite-reopen");
    assert!(
        result.is_ok(),
        "conformance failures:\n{}",
        result.err().unwrap_or_default()
    );
}

#[tokio::test]
async fn default_node_options_open_in_memory_storage() {
    let storage = open_node_sqlite_storage(":memory:", Default::default())
        .await
        .expect("open in-memory storage from default node options");
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");
}

fn namespaced_options(prefix: &str) -> NodeSqliteStorageOptions {
    NodeSqliteStorageOptions {
        prefix: Some(prefix.to_string()),
        ..NodeSqliteStorageOptions::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespaced_sqlite_storage_conformance() {
    let result = run_storage_conformance(|| async {
        let dir = TempDir::new("rpi-durable-sqlite-ns-conf");
        let path = dir
            .path()
            .join("storage.sqlite")
            .to_string_lossy()
            .into_owned();
        let storage = open_node_sqlite_storage(&path, namespaced_options("shared_")).await?;
        Ok(Arc::new(storage) as Arc<dyn Storage>)
    })
    .await;
    cleanup_temp_dirs("rpi-durable-sqlite-ns-conf");
    assert!(
        result.is_ok(),
        "conformance failures:\n{}",
        result.err().unwrap_or_default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespaced_sqlite_storage_conformance_across_reopen() {
    let result = run_storage_conformance(|| async {
        let dir = TempDir::new("rpi-durable-sqlite-ns-reopen");
        let path = dir
            .path()
            .join("storage.sqlite")
            .to_string_lossy()
            .into_owned();
        let storage = open_node_sqlite_storage(&path, namespaced_options("shared_")).await?;
        let reopen_path = path.clone();
        let reopened = ReopeningStorage::new(Box::new(storage), move || {
            let path = reopen_path.clone();
            Box::pin(async move {
                let storage =
                    open_node_sqlite_storage(&path, namespaced_options("shared_")).await?;
                Ok(Box::new(storage) as Box<dyn Storage>)
            })
        });
        Ok(Arc::new(reopened) as Arc<dyn Storage>)
    })
    .await;
    cleanup_temp_dirs("rpi-durable-sqlite-ns-reopen");
    assert!(
        result.is_ok(),
        "conformance failures:\n{}",
        result.err().unwrap_or_default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_namespaced_storages_commit_concurrently_in_one_database() {
    let dir = TempDir::new("rpi-durable-sqlite-prefix-concurrent");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let first = open_node_sqlite_storage(&path, namespaced_options("first_"))
        .await
        .expect("open first_");
    let second = open_node_sqlite_storage(&path, namespaced_options("second_"))
        .await
        .expect("open second_");
    let entry_id: EntryId = id_from_number(2);
    let root = StorageWrite::Conversation {
        value: ConversationRecord {
            id: ROOT_CONVERSATION_ID,
            parent: None,
            owner: None,
        },
    };
    let first_writes = [
        root.clone(),
        StorageWrite::Entry {
            value: EntryRecord::new(entry_id, ROOT_CONVERSATION_ID, "first"),
        },
    ];
    let second_writes = [
        root,
        StorageWrite::Entry {
            value: EntryRecord::new(entry_id, ROOT_CONVERSATION_ID, "second"),
        },
    ];
    let commit_first = first.commit(&first_writes, &BACKGROUND_CONTEXT);
    let commit_second = second.commit(&second_writes, &BACKGROUND_CONTEXT);
    let (first_result, second_result) = tokio::join!(commit_first, commit_second);
    first_result.expect("first commit");
    second_result.expect("second commit");
    assert_eq!(
        first
            .entry(entry_id, &BACKGROUND_CONTEXT)
            .await
            .expect("first entry")
            .expect("present")
            .entry
            .kind,
        "first"
    );
    assert_eq!(
        second
            .entry(entry_id, &BACKGROUND_CONTEXT)
            .await
            .expect("second entry")
            .expect("present")
            .entry
            .kind,
        "second"
    );
    first.close(&BACKGROUND_CONTEXT).await.expect("close first");
    second
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("close second");
    cleanup_temp_dirs("rpi-durable-sqlite-prefix-concurrent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_storage_conformance_after_version_one_migration() {
    let result = run_storage_conformance(|| async {
        let dir = TempDir::new("rpi-durable-sqlite-v1-conf");
        let path = dir
            .path()
            .join("storage.sqlite")
            .to_string_lossy()
            .into_owned();
        let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default()).await?;
        let migrations = sqlite_migrations();
        apply_sqlite_migrations(&database, &migrations[..1]).await?;
        database.close().await?;
        let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default()).await?;
        Ok(Arc::new(storage) as Arc<dyn Storage>)
    })
    .await;
    cleanup_temp_dirs("rpi-durable-sqlite-v1-conf");
    assert!(
        result.is_ok(),
        "conformance failures:\n{}",
        result.err().unwrap_or_default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespaced_storage_shares_a_database_with_host_tables_and_other_prefixes() {
    let dir = TempDir::new("rpi-durable-sqlite-prefix");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let host = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open host");
    host.exec("CREATE TABLE tasks (text TEXT) STRICT")
        .await
        .expect("host table");
    host.run("INSERT INTO tasks (text) VALUES ('host')", vec![])
        .await
        .expect("host row");
    host.close().await.expect("close host");

    let entry_id: EntryId = id_from_number(2);
    let storage = open_node_sqlite_storage(&path, namespaced_options("rpi_"))
        .await
        .expect("open rpi_ storage");
    storage
        .commit(
            &[
                StorageWrite::Conversation {
                    value: ConversationRecord {
                        id: ROOT_CONVERSATION_ID,
                        parent: None,
                        owner: None,
                    },
                },
                StorageWrite::Entry {
                    value: EntryRecord::new(entry_id, ROOT_CONVERSATION_ID, "namespaced"),
                },
            ],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("commit rpi_");
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");

    let reopened = open_node_sqlite_storage(&path, namespaced_options("rpi_"))
        .await
        .expect("reopen rpi_");
    assert!(
        reopened
            .conversation(ROOT_CONVERSATION_ID, &BACKGROUND_CONTEXT)
            .await
            .expect("conversation")
            .is_some()
    );
    assert_eq!(
        reopened
            .entry(entry_id, &BACKGROUND_CONTEXT)
            .await
            .expect("entry")
            .expect("present")
            .entry
            .kind,
        "namespaced"
    );
    reopened.close(&BACKGROUND_CONTEXT).await.expect("close");

    let other = open_node_sqlite_storage(&path, namespaced_options("other_"))
        .await
        .expect("open other_ storage");
    assert!(
        other
            .conversation(ROOT_CONVERSATION_ID, &BACKGROUND_CONTEXT)
            .await
            .expect("conversation")
            .is_none(),
        "another prefix must not see rpi_ rows"
    );
    assert!(
        other
            .entry(entry_id, &BACKGROUND_CONTEXT)
            .await
            .expect("entry")
            .is_none(),
        "another prefix must not see rpi_ entries"
    );
    other
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
        .expect("commit other_");
    other.close(&BACKGROUND_CONTEXT).await.expect("close");

    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("raw open");
    let names = table_names(&database).await;
    assert!(names.contains(&"tasks".to_string()), "{names:?}");
    assert!(names.contains(&"rpi_tasks".to_string()), "{names:?}");
    assert!(names.contains(&"other_tasks".to_string()), "{names:?}");
    let unprefixed: Vec<&String> = names
        .iter()
        .filter(|name| !name.starts_with("rpi_") && !name.starts_with("other_"))
        .collect();
    assert_eq!(unprefixed, vec![&"tasks".to_string()], "{names:?}");
    assert_eq!(
        database
            .get_i64("SELECT count(*) FROM rpi_entries", vec![])
            .await
            .expect("rpi entries")
            .expect("row"),
        1
    );
    assert_eq!(
        database
            .get_i64("SELECT count(*) FROM other_entries", vec![])
            .await
            .expect("other entries")
            .expect("row"),
        0
    );
    assert_eq!(
        database
            .get_i64("SELECT count(*) FROM tasks", vec![])
            .await
            .expect("host rows")
            .expect("row"),
        1
    );
    assert_eq!(
        database
            .get_i64("SELECT count(*) FROM rpi_tasks", vec![])
            .await
            .expect("namespaced tasks")
            .expect("row"),
        0
    );
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-prefix");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_or_reserved_prefix_fails_before_creating_the_database() {
    let dir = TempDir::new("rpi-durable-sqlite-prefix-invalid");
    let nested = dir.path().join("nested");
    let path = nested.join("storage.sqlite");
    for prefix in ["", "bad-prefix", "sqlite_rpi_"] {
        let result =
            open_node_sqlite_storage(&path.to_string_lossy(), namespaced_options(prefix)).await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("prefix {prefix:?} must be rejected"),
        };
        let expected = if prefix.eq_ignore_ascii_case("sqlite_rpi_") {
            "is reserved"
        } else {
            "Invalid SQLite schema prefix"
        };
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert!(
        !nested.exists(),
        "an invalid prefix must fail before creating the database directory"
    );
    cleanup_temp_dirs("rpi-durable-sqlite-prefix-invalid");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_sets_wal_pragmas_and_defaults() {
    let dir = TempDir::new("rpi-durable-sqlite-pragma");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open database");
    let (journal_mode, synchronous, busy_timeout, autocheckpoint) = database
        .with_connection(|conn| {
            let journal_mode: String =
                conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
            let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
            let busy_timeout: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            let autocheckpoint: i64 =
                conn.query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0))?;
            Ok((journal_mode, synchronous, busy_timeout, autocheckpoint))
        })
        .await
        .expect("pragmas");
    assert_eq!(journal_mode.to_lowercase(), "wal");
    assert_eq!(synchronous, 1, "synchronous = NORMAL");
    assert_eq!(busy_timeout, 5000);
    assert_eq!(autocheckpoint, 1000);
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-pragma");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn survives_kill_minus_nine_and_continues_after_reopen() {
    let dir = TempDir::new("rpi-durable-sqlite-crash");
    let path = dir.path().join("storage.sqlite");
    let marker = dir.path().join("ready");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rpi-durable-crash-writer"))
        .arg(path.to_string_lossy().into_owned())
        .arg(marker.to_string_lossy().into_owned())
        .spawn()
        .expect("spawn crash writer");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !marker.exists() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("crash writer did not signal readiness");
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("crash writer exited early: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let entry_id: EntryId = id_from_number(
        std::fs::read_to_string(&marker)
            .expect("marker")
            .trim()
            .parse()
            .expect("id"),
    );
    // SIGKILL on Unix (std::process::Child::kill).
    child.kill().expect("kill");
    let _ = child.wait();

    let storage =
        open_node_sqlite_storage(&path.to_string_lossy(), NodeSqliteStorageOptions::default())
            .await
            .expect("reopen after crash");
    let stored = storage
        .entry(entry_id, &BACKGROUND_CONTEXT)
        .await
        .expect("read")
        .expect("entry survives");
    assert_eq!(stored.entry.kind, "crash-entry");
    let next: EntryId = id_from_number(storage.mint_id().await.expect("mint"));
    assert!(next.get() > entry_id.get());
    storage
        .commit(
            &[StorageWrite::Entry {
                value: EntryRecord::new(next, ROOT_CONVERSATION_ID, "after-crash"),
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("commit after crash");
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-crash");
}

async fn schema_version(database: &SqliteDatabase) -> u64 {
    database
        .get_i64(
            "SELECT version FROM durable_schema WHERE singleton = 1",
            vec![],
        )
        .await
        .expect("version")
        .expect("row") as u64
}

async fn tasks_index_names(database: &SqliteDatabase) -> Vec<String> {
    database
        .get::<Vec<String>>(
            "SELECT json_group_array(name) FROM (SELECT name FROM sqlite_schema \
             WHERE type = 'index' AND tbl_name = 'tasks' ORDER BY name)",
            vec![],
        )
        .await
        .expect("index names")
        .expect("row")
}

async fn table_names(database: &SqliteDatabase) -> Vec<String> {
    database
        .get::<Vec<String>>(
            "SELECT json_group_array(name) FROM (SELECT name FROM sqlite_schema \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name)",
            vec![],
        )
        .await
        .expect("table names")
        .expect("row")
}

async fn table_exists(database: &SqliteDatabase, name: &str) -> bool {
    database
        .get_i64(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?",
            vec![name.into()],
        )
        .await
        .expect("table count")
        .expect("row")
        > 0
}

async fn insert_task_row(database: &SqliteDatabase, task: &TaskRecord) {
    database
        .run(
            "INSERT INTO tasks (id, conversation_id, kind, status, abort_requested, background, record) \
             VALUES (?, ?, ?, ?, 0, 0, ?)",
            vec![
                task.id.get().into(),
                task.conversation_id.get().into(),
                task.kind.as_str().into(),
                task.status().as_str().into(),
                serde_json::to_string(task).expect("task json").into(),
            ],
        )
        .await
        .expect("insert task");
}

async fn insert_sleeping_task(database: &SqliteDatabase) -> Result<(), StorageError> {
    database
        .run(
            "INSERT INTO tasks (id, conversation_id, kind, status, abort_requested, background, record) \
             VALUES (6, 1, 'test.task', 'sleeping', 0, 0, '{}')",
            vec![],
        )
        .await
}

fn waiting_task(id: u64) -> TaskRecord {
    TaskRecord {
        id: id_from_number(id),
        conversation_id: ROOT_CONVERSATION_ID,
        kind: "test.task".to_string(),
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
        started_at: None,
        ended_at: None,
        state: TaskState::Waiting {
            checkpoint: serde_json::json!({ "phase": "run" }),
            on: Vec::new(),
            policy: JoinPolicy::AllSettled,
        },
        memos: None,
    }
}

/// Build a version 1 database (pre-sleeping task table) with one waiting task.
async fn version_one_database(dir: &TempDir) -> (String, TaskRecord) {
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    let migrations = sqlite_migrations();
    apply_sqlite_migrations(&database, &migrations[..1])
        .await
        .expect("apply v1");
    let task = waiting_task(5);
    insert_task_row(&database, &task).await;
    assert!(
        insert_sleeping_task(&database).await.is_err(),
        "the v1 task table must reject the sleeping status"
    );
    database.close().await.expect("close");
    (path, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrations_apply_repeatedly_and_older_or_newer_binaries_are_rejected() {
    let dir = TempDir::new("rpi-durable-sqlite-migrations");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    let migrations = sqlite_migrations();
    apply_sqlite_migrations(&database, &migrations)
        .await
        .expect("first apply");
    apply_sqlite_migrations(&database, &migrations)
        .await
        .expect("second apply");
    assert_eq!(CURRENT_SQLITE_SCHEMA_VERSION, 2);
    assert_eq!(
        schema_version(&database).await,
        CURRENT_SQLITE_SCHEMA_VERSION
    );
    // A 1.3.0-or-earlier binary (v1 history only) refuses a version 2 database.
    let older_history = create_sqlite_migrations(&DEFAULT_SQLITE_SCHEMA);
    let result = apply_sqlite_migrations(&database, &older_history[..1]).await;
    match result {
        Err(error) => assert!(
            error
                .to_string()
                .contains("is newer than supported version 1"),
            "{error}"
        ),
        Ok(()) => panic!("expected the older history to reject version 2"),
    }
    database
        .run(
            "UPDATE durable_schema SET version = ? WHERE singleton = 1",
            vec![SqliteValue::Integer(3)],
        )
        .await
        .expect("bump version");
    database.close().await.expect("close");
    let result = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default()).await;
    match result {
        Err(error) => assert!(
            error
                .to_string()
                .contains("is newer than supported version 2"),
            "{error}"
        ),
        Ok(_) => panic!("expected newer-version rejection"),
    }
    cleanup_temp_dirs("rpi-durable-sqlite-migrations");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_pending_migration_rolls_back() {
    let dir = TempDir::new("rpi-durable-sqlite-migration-rollback");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    // Commit the current schema first so the failed pending migration rolls
    // back only its own statements (upstream retry test shape).
    let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
        .await
        .expect("open storage");
    storage
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("close storage");
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    let mut migrations = sqlite_migrations();
    migrations.push(SqliteMigration {
        version: CURRENT_SQLITE_SCHEMA_VERSION + 1,
        statements: vec![
            "CREATE TABLE migration_probe (value TEXT) STRICT".to_string(),
            "THIS IS NOT SQL".to_string(),
        ],
    });
    let result = apply_sqlite_migrations(&database, &migrations).await;
    assert!(result.is_err(), "expected failed migration");
    assert_eq!(
        schema_version(&database).await,
        CURRENT_SQLITE_SCHEMA_VERSION
    );
    assert!(!table_exists(&database, "migration_probe").await);
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-migration-rollback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_one_database_migrates_and_keeps_tasks_and_indexes() {
    let dir = TempDir::new("rpi-durable-sqlite-v1-migration");
    let (path, task) = version_one_database(&dir).await;

    // Opening storage applies the v2 rebuild; reopened storage does not repeat
    // it and keeps serving the same task row.
    for _ in 0..2 {
        let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
            .await
            .expect("open migrated storage");
        let stored = storage
            .task(task.id, &BACKGROUND_CONTEXT)
            .await
            .expect("read task")
            .expect("task retained");
        assert_eq!(stored, task);
        storage.close(&BACKGROUND_CONTEXT).await.expect("close");
    }

    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("raw open");
    assert_eq!(schema_version(&database).await, 2);
    assert_eq!(
        database
            .get_text("SELECT record FROM tasks WHERE id = 5", vec![])
            .await
            .expect("task row")
            .expect("row"),
        serde_json::to_string(&task).expect("task json")
    );
    insert_sleeping_task(&database)
        .await
        .expect("version 2 accepts sleeping tasks");
    assert_eq!(
        tasks_index_names(&database).await,
        vec![
            "tasks_by_abort_requested",
            "tasks_by_background",
            "tasks_by_conversation",
            "tasks_by_kind",
            "tasks_by_status",
        ]
    );
    assert!(!table_exists(&database, "tasks_v2").await);
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-v1-migration");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_version_two_rebuild_rolls_back_the_task_table() {
    let dir = TempDir::new("rpi-durable-sqlite-v2-rollback");
    let (path, task) = version_one_database(&dir).await;
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    let mut migrations = sqlite_migrations();
    migrations[1].statements.push("THIS IS NOT SQL".to_string());
    assert!(
        apply_sqlite_migrations(&database, &migrations)
            .await
            .is_err(),
        "expected the v2 migration to fail"
    );
    assert_eq!(schema_version(&database).await, 1);
    assert_eq!(
        database
            .get_text("SELECT record FROM tasks WHERE id = 5", vec![])
            .await
            .expect("task row")
            .expect("row"),
        serde_json::to_string(&task).expect("task json")
    );
    assert!(!table_exists(&database, "tasks_v2").await);
    assert!(
        insert_sleeping_task(&database).await.is_err(),
        "a failed rebuild must leave the v1 task table in place"
    );

    // The same database still migrates on the next open.
    apply_sqlite_migrations(&database, &sqlite_migrations())
        .await
        .expect("retry migration");
    assert_eq!(schema_version(&database).await, 2);
    insert_sleeping_task(&database)
        .await
        .expect("retry accepts sleeping tasks");
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-v2-rollback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_version_two_migration_rolls_back_to_version_one() {
    let dir = TempDir::new("rpi-durable-sqlite-v2-kill");
    let path = dir.path().join("storage.sqlite");
    let marker = dir.path().join("ready");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rpi-durable-crash-writer"))
        .arg(path.to_string_lossy().into_owned())
        .arg(marker.to_string_lossy().into_owned())
        .arg("migrate")
        .spawn()
        .expect("spawn migration writer");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !marker.exists() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("migration writer did not signal readiness");
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("migration writer exited early: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The child starts the v2 transaction right after the marker; the appended
    // recursive CTE keeps it running so the SIGKILL lands mid-migration.
    std::thread::sleep(std::time::Duration::from_millis(150));
    child.kill().expect("kill");
    let _ = child.wait();

    let database = SqliteDatabase::open(&path.to_string_lossy(), SqliteDatabaseOptions::default())
        .await
        .expect("reopen after crash");
    assert_eq!(
        schema_version(&database).await,
        1,
        "an interrupted migration must roll back to version 1"
    );
    assert!(
        database
            .get_text("SELECT record FROM tasks WHERE id = 5", vec![])
            .await
            .expect("task row")
            .is_some(),
        "the v1 task row survives the interrupted rebuild"
    );
    assert!(!table_exists(&database, "tasks_v2").await);
    assert!(
        insert_sleeping_task(&database).await.is_err(),
        "the interrupted rebuild must not expose a half-migrated task table"
    );
    database.close().await.expect("close");

    let storage =
        open_node_sqlite_storage(&path.to_string_lossy(), NodeSqliteStorageOptions::default())
            .await
            .expect("open migrates after the interrupted attempt");
    assert!(
        storage
            .task(id_from_number(5), &BACKGROUND_CONTEXT)
            .await
            .expect("read task")
            .is_some()
    );
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-v2-kill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_contiguous_and_corrupt_versions_are_rejected() {
    let dir = TempDir::new("rpi-durable-sqlite-version-corruption");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    let v1 = sqlite_migrations().remove(0);
    let non_contiguous = vec![
        v1.clone(),
        SqliteMigration {
            version: 3,
            statements: vec!["CREATE TABLE skipped (value TEXT) STRICT".to_string()],
        },
    ];
    let result = apply_sqlite_migrations(&database, &non_contiguous).await;
    assert!(
        result
            .expect_err("non-contiguous")
            .to_string()
            .contains("contiguous versions"),
    );
    apply_sqlite_migrations(&database, &[v1]).await.expect("v1");
    // Losing the version row leaves a populated schema that cannot be migrated
    // from scratch; the failure rolls back and keeps the existing tables.
    database
        .run("DELETE FROM durable_schema", vec![])
        .await
        .expect("drop version");
    let result = apply_sqlite_migrations(&database, &sqlite_migrations()).await;
    assert!(result.is_err(), "expected corrupt-metadata rejection");
    assert!(table_exists(&database, "tasks").await);
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-version-corruption");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_missing_metadata_on_reopen() {
    let dir = TempDir::new("rpi-durable-sqlite-metadata");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
        .await
        .expect("open");
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("raw open");
    database
        .run("DELETE FROM durable_metadata", vec![])
        .await
        .expect("delete metadata");
    database.close().await.expect("close");
    let result = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default()).await;
    match result {
        Err(error) => assert!(
            error
                .to_string()
                .contains("Durable SQLite metadata is missing"),
            "{error}"
        ),
        Ok(_) => panic!("expected metadata rejection"),
    }
    cleanup_temp_dirs("rpi-durable-sqlite-metadata");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_reads_observe_one_committed_state_across_replacements() {
    let dir = TempDir::new("rpi-durable-sqlite-doc-consistency");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let storage = Arc::new(
        open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
            .await
            .expect("open"),
    );
    let id: DocumentId = id_from_number(storage.mint_id().await.expect("mint"));
    storage
        .commit(
            &[StorageWrite::DocumentCreate {
                record: DocumentCreate {
                    id,
                    kind: "replaced".to_string(),
                    key: None,
                    scope: DocumentScope::Session,
                    history: None,
                    fork: None,
                },
                content: DocumentContent::Base {
                    version: 1,
                    value: serde_json::json!({ "value": 1 })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                },
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("create");
    let reader = Arc::clone(&storage);
    let read = tokio::spawn(async move {
        reader
            .document(
                id,
                rpi_durable::types::DocumentPoint::CURRENT,
                &BACKGROUND_CONTEXT,
            )
            .await
    });
    let replace = storage
        .commit(
            &[StorageWrite::DocumentChange {
                id,
                content: DocumentContent::Base {
                    version: 1,
                    value: serde_json::json!({ "value": 2 })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                },
            }],
            &BACKGROUND_CONTEXT,
        )
        .await;
    let stored = read.await.expect("join").expect("read").expect("document");
    replace.expect("replace");
    let value = stored.value.get("value").and_then(|value| value.as_i64());
    assert!(
        matches!(value, Some(1) | Some(2)),
        "unexpected value: {value:?}"
    );
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-doc-consistency");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_transaction_rolls_back_and_the_next_transaction_starts() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-cancel");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    database
        .exec("CREATE TABLE probe (value INTEGER)")
        .await
        .expect("table");

    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(25),
        database.transaction(|tx| async move {
            tx.run(
                "INSERT INTO probe (value) VALUES (?)",
                vec![SqliteValue::Integer(1)],
            )
            .await?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Ok(())
        }),
    )
    .await;
    assert!(cancelled.is_err(), "the transaction must be cancelled");

    // The uncommitted INSERT must not be visible after the rollback guard ran.
    let count = database
        .get_i64("SELECT count(*) FROM probe", vec![])
        .await
        .expect("read after cancel")
        .expect("row");
    assert_eq!(count, 0, "uncommitted write leaked after cancellation");

    let count: i64 = database
        .transaction(|tx| async move {
            tx.run(
                "INSERT INTO probe (value) VALUES (?)",
                vec![SqliteValue::Integer(2)],
            )
            .await?;
            tx.get_i64("SELECT count(*) FROM probe", vec![])
                .await?
                .ok_or_else(|| StorageError::message("missing row"))
        })
        .await
        .expect("next transaction must start cleanly");
    assert_eq!(count, 1);

    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-facade-cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_transaction_handle_expires_and_rolls_back_on_callback_error() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-rollback");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    database
        .exec("CREATE TABLE probe (value INTEGER)")
        .await
        .expect("table");
    let outcome: Result<(), StorageError> = database
        .transaction(|tx| async move {
            tx.run(
                "INSERT INTO probe (value) VALUES (?)",
                vec![SqliteValue::Integer(1)],
            )
            .await?;
            Err(StorageError::message("roll back"))
        })
        .await;
    assert_eq!(outcome.expect_err("rollback").to_string(), "roll back");
    let count = database
        .get_i64("SELECT count(*) FROM probe", vec![])
        .await
        .expect("count")
        .expect("row");
    assert_eq!(count, 0);

    let value: i64 = database
        .transaction(|tx| async move {
            tx.run(
                "INSERT INTO probe (value) VALUES (?)",
                vec![SqliteValue::Integer(2)],
            )
            .await?;
            tx.get_i64(
                "SELECT value FROM probe ORDER BY rowid DESC LIMIT 1",
                vec![],
            )
            .await?
            .ok_or_else(|| StorageError::message("missing row"))
        })
        .await
        .expect("commit");
    assert_eq!(value, 2);
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-facade-rollback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_stale_handle_rejects_after_settlement() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-stale");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    database
        .exec("CREATE TABLE stale_probe (value INTEGER)")
        .await
        .expect("table");
    let handle = std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let captured = std::sync::Arc::clone(&handle);
    database
        .transaction(move |tx| async move {
            tx.run(
                "INSERT INTO stale_probe (value) VALUES (?)",
                vec![SqliteValue::Integer(1)],
            )
            .await?;
            *captured.lock().await = Some(tx);
            Ok(())
        })
        .await
        .expect("transaction");
    let guard = handle.lock().await;
    let transaction = guard.as_ref().expect("handle");
    let error = transaction
        .run(
            "INSERT INTO stale_probe (value) VALUES (?)",
            vec![SqliteValue::Integer(2)],
        )
        .await;
    assert_eq!(
        error.expect_err("stale").to_string(),
        "SQLite transaction handle is no longer active"
    );
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-facade-stale");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_reentrant_database_call_reports_explicit_error() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-reentrant");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    let database_for_callback = database.clone_handle();
    let outcome: Result<(), StorageError> = database
        .transaction(move |_tx| async move {
            // Upstream deadlocks here; rpi replaces that with an explicit error.
            database_for_callback.exec("SELECT 1").await
        })
        .await;
    assert_eq!(
        outcome.expect_err("reentrant").to_string(),
        "SQLite database cannot be used from inside its own transaction callback"
    );
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-facade-reentrant");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_defers_ids_until_commit_settles() {
    let dir = TempDir::new("rpi-durable-sqlite-id-adoption");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
        .await
        .expect("open");
    let entry_id: EntryId = id_from_number(100);
    storage
        .commit(
            &[StorageWrite::Entry {
                value: EntryRecord::new(entry_id, ROOT_CONVERSATION_ID, "settled"),
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("commit");
    let next: EntryId = id_from_number(storage.mint_id().await.expect("mint"));
    assert_eq!(next.get(), 101);
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-id-adoption");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_publication_is_atomic_with_secondary_indexes() {
    let dir = TempDir::new("rpi-durable-sqlite-atomic");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
        .await
        .expect("open");
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
        .expect("root");
    let entry_id: EntryId = id_from_number(storage.mint_id().await.expect("mint"));
    let result = storage
        .commit(
            &[
                StorageWrite::Entry {
                    value: EntryRecord::new(entry_id, ROOT_CONVERSATION_ID, "transient"),
                },
                StorageWrite::Conversation {
                    value: ConversationRecord {
                        id: ROOT_CONVERSATION_ID,
                        parent: None,
                        owner: None,
                    },
                },
            ],
            &BACKGROUND_CONTEXT,
        )
        .await;
    assert!(result.is_err());
    assert!(
        storage
            .entry(entry_id, &BACKGROUND_CONTEXT)
            .await
            .expect("read")
            .is_none()
    );
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-atomic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_reports_rollback_failure_with_a_distinct_error() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-rollback-failure");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    database
        .exec("CREATE TABLE rollback_probe (value INTEGER)")
        .await
        .expect("table");
    let outcome: Result<(), StorageError> = database
        .transaction(|tx| async move {
            tx.run(
                "INSERT INTO rollback_probe (value) VALUES (?)",
                vec![SqliteValue::Integer(1)],
            )
            .await?;
            // Escape the transaction: the facade's rollback now fails, so the
            // callback error must not be presented as a guaranteed rollback.
            tx.exec("COMMIT").await?;
            Err(StorageError::message("rejected after an escaped commit"))
        })
        .await;
    match outcome {
        Err(StorageError::RollbackFailed { callback, rollback }) => {
            assert!(
                callback
                    .to_string()
                    .contains("rejected after an escaped commit")
            );
            assert!(!rollback.to_string().is_empty());
        }
        other => panic!("expected RollbackFailed, got {other:?}"),
    }
    let count = database
        .get_i64("SELECT count(*) FROM rollback_probe", vec![])
        .await
        .expect("count")
        .expect("row");
    assert_eq!(count, 1, "the escaped commit survives");
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-facade-rollback-failure");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_queues_ordinary_operations_behind_an_active_transaction() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-queue");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    database
        .exec("CREATE TABLE operation_queue (value INTEGER)")
        .await
        .expect("table");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let transaction_db = database.clone_handle();
    let transaction = tokio::spawn(async move {
        transaction_db
            .transaction(move |tx| async move {
                tx.run(
                    "INSERT INTO operation_queue (value) VALUES (?)",
                    vec![SqliteValue::Integer(1)],
                )
                .await?;
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok(())
            })
            .await
    });
    started_rx.await.expect("transaction started");
    let queued_db = database.clone_handle();
    let queued = tokio::spawn(async move {
        queued_db
            .run(
                "INSERT INTO operation_queue (value) VALUES (?)",
                vec![SqliteValue::Integer(2)],
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !queued.is_finished(),
        "ordinary operation must queue behind the transaction"
    );
    let _ = release_tx.send(());
    transaction
        .await
        .expect("join transaction")
        .expect("transaction");
    queued.await.expect("join queued").expect("queued write");
    let rows: Vec<serde_json::Value> = database
        .all(
            "SELECT json_object('value', value) FROM operation_queue ORDER BY value",
            vec![],
        )
        .await
        .expect("rows");
    assert_eq!(
        rows,
        vec![
            serde_json::json!({ "value": 1 }),
            serde_json::json!({ "value": 2 })
        ]
    );
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-facade-queue");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_multi_query_reads_finish_before_storage_closes() {
    let dir = TempDir::new("rpi-durable-sqlite-admitted-reads");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let storage = Arc::new(
        open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
            .await
            .expect("open"),
    );
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
        .expect("root");
    let entry_id: EntryId = id_from_number(storage.mint_id().await.expect("mint"));
    storage
        .commit(
            &[StorageWrite::Entry {
                value: EntryRecord::new(entry_id, ROOT_CONVERSATION_ID, "probe"),
            }],
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("entry");

    // Poll each multi-query read once so it is admitted before close starts.
    let mut scan = Box::pin(storage.scan_entries(
        rpi_durable::types::EntryQuery {
            conversation_id: ROOT_CONVERSATION_ID,
            min_entry_id: None,
            max_entry_id: None,
            order: None,
        },
        10,
        None,
        &BACKGROUND_CONTEXT,
    ));
    let mut conversation_entry = Box::pin(storage.entry_in_conversation(
        ROOT_CONVERSATION_ID,
        entry_id,
        &BACKGROUND_CONTEXT,
    ));
    let mut head =
        Box::pin(storage.find_latest_head_marker(ROOT_CONVERSATION_ID, None, &BACKGROUND_CONTEXT));
    assert!(futures::poll!(scan.as_mut()).is_pending());
    assert!(futures::poll!(conversation_entry.as_mut()).is_pending());
    assert!(futures::poll!(head.as_mut()).is_pending());

    // Drive the reads, the close and a repeated close concurrently: the reads
    // are already admitted, so close must wait for them.
    let closer = Arc::clone(&storage);
    let repeated = Arc::clone(&storage);
    let (scan_page, entry, head, close, close_again) = tokio::join!(
        scan,
        conversation_entry,
        head,
        async move { closer.close(&BACKGROUND_CONTEXT).await },
        async move { repeated.close(&BACKGROUND_CONTEXT).await },
    );

    let scan_page = scan_page.expect("scan");
    assert_eq!(
        scan_page
            .items
            .iter()
            .map(|item| item.id.get())
            .collect::<Vec<_>>(),
        vec![entry_id.get()]
    );
    let entry = entry.expect("entry").expect("present");
    assert_eq!(entry.entry.kind, "probe");
    assert!(head.expect("head").is_none());
    close.expect("close");
    close_again.expect("repeated close");

    let closed = storage
        .scan_entries(
            rpi_durable::types::EntryQuery {
                conversation_id: ROOT_CONVERSATION_ID,
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            10,
            None,
            &BACKGROUND_CONTEXT,
        )
        .await;
    assert!(
        closed
            .expect_err("closed read")
            .to_string()
            .contains("SqliteStorage is closed"),
        "later reads must reject after close"
    );
    cleanup_temp_dirs("rpi-durable-sqlite-admitted-reads");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_reentrant_close_reports_explicit_error() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-reentrant-close");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    let database_for_callback = database.clone_handle();
    let outcome: Result<(), StorageError> = database
        .transaction(move |_tx| async move { database_for_callback.close().await })
        .await;
    assert_eq!(
        outcome.expect_err("reentrant close").to_string(),
        "SQLite database cannot be used from inside its own transaction callback"
    );
    // The database is still usable after the failed reentrant call.
    database.exec("SELECT 1").await.expect("post-check");
    database.close().await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-facade-reentrant-close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_cross_task_close_queues_behind_an_active_transaction() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-close-queue");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    database
        .exec("CREATE TABLE close_queue (value INTEGER)")
        .await
        .expect("table");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let transaction_db = database.clone_handle();
    let transaction = tokio::spawn(async move {
        transaction_db
            .transaction(move |tx| async move {
                tx.run(
                    "INSERT INTO close_queue (value) VALUES (?)",
                    vec![SqliteValue::Integer(1)],
                )
                .await?;
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok(())
            })
            .await
    });
    started_rx.await.expect("transaction started");
    let closer = database.clone_handle();
    let close = tokio::spawn(async move { closer.close().await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !close.is_finished(),
        "close must queue behind the active transaction"
    );
    let _ = release_tx.send(());
    transaction
        .await
        .expect("join transaction")
        .expect("transaction");
    close.await.expect("join close").expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-facade-close-queue");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_document_scan_rejects_cursors_beyond_the_safe_integer() {
    let dir = TempDir::new("rpi-durable-sqlite-unsafe-cursor");
    let path = dir
        .path()
        .join("storage.sqlite")
        .to_string_lossy()
        .into_owned();
    let storage = open_node_sqlite_storage(&path, NodeSqliteStorageOptions::default())
        .await
        .expect("open");
    let cursor = rpi_durable::types::Cursor {
        after: 9_007_199_254_740_992,
        order: None,
    };
    let result = storage
        .scan_documents(
            rpi_durable::types::DocumentQuery {
                scope: rpi_durable::types::DocumentScope::Session,
                at: rpi_durable::types::DocumentPoint::CURRENT,
                kind: None,
            },
            10,
            Some(cursor),
            &BACKGROUND_CONTEXT,
        )
        .await;
    assert!(
        result
            .expect_err("invalid cursor")
            .to_string()
            .contains("Invalid storage cursor"),
        "document scans must reject unsafe cursor IDs"
    );
    storage.close(&BACKGROUND_CONTEXT).await.expect("close");
    cleanup_temp_dirs("rpi-durable-sqlite-unsafe-cursor");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_serves_operations_queued_before_close() {
    let dir = TempDir::new("rpi-durable-sqlite-facade-close-fifo");
    let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
    let database = SqliteDatabase::open(&path, SqliteDatabaseOptions::default())
        .await
        .expect("open");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let transaction_db = database.clone_handle();
    let transaction = tokio::spawn(async move {
        transaction_db
            .transaction(move |_tx| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok(())
            })
            .await
    });
    started_rx.await.expect("transaction started");

    // Queue a read and then a close behind the parked transaction.
    let mut queued = Box::pin(database.get_text("SELECT 'ok'", vec![]));
    assert!(futures::poll!(queued.as_mut()).is_pending());
    let closer = database.clone_handle();
    let mut close = Box::pin(async move { closer.close().await });
    assert!(futures::poll!(close.as_mut()).is_pending());

    let _ = release_tx.send(());
    transaction
        .await
        .expect("join transaction")
        .expect("transaction");
    // The read queued before close is served; close then settles.
    assert_eq!(queued.await.expect("queued read").as_deref(), Some("ok"));
    close.await.expect("close");
    // Operations after close reject as closed.
    let after = database.get_text("SELECT 'late'", vec![]).await;
    assert!(
        after
            .expect_err("post-close read")
            .to_string()
            .contains("SQLite database is closed"),
        "operations after close must reject"
    );
    cleanup_temp_dirs("rpi-durable-sqlite-facade-close-fifo");
}
