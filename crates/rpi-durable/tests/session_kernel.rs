//! Session-kernel integration tests (port of the session domain of
//! `packages/durable/test/session-*.test.ts` @ `b4afdddf`).
//!
//! Each test cites the upstream case it adapts. Rust asserts value equality
//! where upstream asserts reference identity (V17-14 §7.3).

mod session_support;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rpi_durable::chord::context::{BACKGROUND_CONTEXT, with_abort_signal};
use rpi_durable::chord::delta::Op;
use rpi_durable::chord::json::{JsonObject, JsonValue};
use rpi_durable::documents::{DocDefinition, DocScope, DocToken, define_doc, define_doc_family};
use rpi_durable::errors::{StorageError, StorageRequestError};
use rpi_durable::harness::{ConversationViews, ConversationWindow, LIVE_DOC};
use rpi_durable::ids::EntryId;
use rpi_durable::session::{
    CommitObserver, CommitOptions, CommitPersistence, CommitPublication, DocOwner, EntryDraft,
    SessionEnd, SessionError, WatchCall, WatchEnd, commit_fn, watch_listener,
};
use rpi_durable::types::{
    DocumentAddress, DocumentFork, DocumentHistory, DocumentPoint, DocumentScope,
};
use tokio_util::sync::CancellationToken;

use session_support::{
    commit, create_conversation, document_copy_changes, flush, open_test_session, try_commit,
};

fn object(value: JsonValue) -> JsonObject {
    value.as_object().cloned().unwrap_or_default()
}

async fn settle_line(session: &Arc<rpi_durable::session::SessionImpl>) {
    tokio::task::yield_now().await;
    session
        .read_on_line(|| Box::pin(async { Ok(()) }))
        .await
        .expect("settle line");
}

fn conversation_doc(
    kind: &str,
    version: u64,
    history: DocumentHistory,
    fork: DocumentFork,
    initial: impl Fn(Option<&JsonValue>) -> JsonObject + Send + Sync + 'static,
) -> DocToken {
    define_doc(DocDefinition {
        kind: kind.to_string(),
        version,
        scope: DocScope::Conversation,
        history: Some(history),
        fork: Some(fork),
        family: false,
        initial: Arc::new(initial),
        migrate: None,
        checkpoint_when: Arc::new(|_value, _ops, _revision| false),
    })
}

fn conversation_family(
    kind: &str,
    version: u64,
    history: DocumentHistory,
    fork: DocumentFork,
    initial: impl Fn(Option<&JsonValue>) -> JsonObject + Send + Sync + 'static,
) -> DocToken {
    define_doc_family(DocDefinition {
        kind: kind.to_string(),
        version,
        scope: DocScope::Conversation,
        history: Some(history),
        fork: Some(fork),
        family: true,
        initial: Arc::new(initial),
        migrate: None,
        checkpoint_when: Arc::new(|_value, _ops, _revision| false),
    })
}

fn session_family() -> &'static DocToken {
    static TOKEN: OnceLock<DocToken> = OnceLock::new();
    TOKEN.get_or_init(|| {
        define_doc_family(DocDefinition {
            kind: "test.session.family".to_string(),
            version: 1,
            scope: DocScope::Session,
            history: None,
            fork: None,
            family: true,
            initial: Arc::new(|seed| {
                object(serde_json::json!({"value": seed.cloned().unwrap_or(JsonValue::Null)}))
            }),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, _revision| false),
        })
    })
}

fn session_doc(
    kind: &str,
    version: u64,
    initial: impl Fn(Option<&JsonValue>) -> JsonObject + Send + Sync + 'static,
) -> DocToken {
    define_doc(DocDefinition {
        kind: kind.to_string(),
        version,
        scope: DocScope::Session,
        history: None,
        fork: None,
        family: false,
        initial: Arc::new(initial),
        migrate: None,
        checkpoint_when: Arc::new(|_value, _ops, _revision| false),
    })
}

fn task_doc(
    kind: &str,
    version: u64,
    initial: impl Fn(Option<&JsonValue>) -> JsonObject + Send + Sync + 'static,
) -> DocToken {
    define_doc(DocDefinition {
        kind: kind.to_string(),
        version,
        scope: DocScope::Task,
        history: None,
        fork: None,
        family: false,
        initial: Arc::new(initial),
        migrate: None,
        checkpoint_when: Arc::new(|_value, _ops, _revision| false),
    })
}

/// A conversation-scoped live document with `{message, items, nested:{count}}`.
fn live() -> &'static DocToken {
    static TOKEN: OnceLock<DocToken> = OnceLock::new();
    TOKEN.get_or_init(|| {
        conversation_doc(
            "test.live",
            1,
            DocumentHistory::Latest,
            DocumentFork::Initial,
            |_| object(serde_json::json!({"message": null, "items": [], "nested": {"count": 0}})),
        )
    })
}

fn counter() -> &'static DocToken {
    static TOKEN: OnceLock<DocToken> = OnceLock::new();
    TOKEN.get_or_init(|| {
        session_doc("test.counter", 1, |_| {
            object(serde_json::json!({"count": 0}))
        })
    })
}

// ─── Group 1: atomic commit ───────────────────────────────────────────────

/// session-documents.test.ts:78 creates an initial base on first access.
#[tokio::test]
async fn commit_success_publishes_once() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("hello"));
            Ok(())
        })
    })
    .await;
    assert_eq!(harness.storage.commits.lock().unwrap().len(), 2); // conversation + document
    assert_eq!(harness.publications.lock().unwrap().len(), 2);
    let snapshot = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["message"], serde_json::json!("hello"));
}

/// session-documents.test.ts:228 aborts every change when the callback fails.
#[tokio::test]
async fn callback_failure_leaves_nothing() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let before = harness.storage.commits.lock().unwrap().len();
    let result = try_commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("lost"));
            let _ = tx.doc(counter(), DocOwner::Session, None).await?;
            Err::<(), _>(SessionError::callback("callback failed"))
        })
    })
    .await;
    assert!(result.is_err());
    assert_eq!(harness.storage.commits.lock().unwrap().len(), before);
    let snapshot = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .unwrap();
    assert!(snapshot.is_none());
}

/// V18-06 R-D11: a StorageRequestError from `commit()` is fatal like any other
/// storage error; the calling commit gets the backend's error, later calls get
/// SessionFailed.
#[tokio::test]
async fn storage_request_error_from_commit_fails_the_session() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    harness.storage.fail_next_commit(StorageError::Request(
        rpi_durable::errors::StorageRequestError::new("copy rejected"),
    ));
    let result = try_commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("rejected"));
            Ok(())
        })
    })
    .await;
    assert!(matches!(
        result,
        Err(SessionError::Storage(StorageError::Request(_)))
    ));
    // Nothing retries; the Session failed.
    assert!(matches!(
        harness
            .session
            .snapshot(
                live(),
                DocOwner::Conversation(conversation),
                &BACKGROUND_CONTEXT
            )
            .await,
        Err(SessionError::Failed { .. })
    ));
    assert!(matches!(
        try_commit(&harness.session, |_tx| Box::pin(async { Ok(()) })).await,
        Err(SessionError::Failed { .. })
    ));
}

/// session-documents.test.ts:440 fails the Session after an uncertain Storage failure.
#[tokio::test]
async fn storage_error_fails_the_session() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    harness
        .storage
        .fail_next_commit(StorageError::message("disk vanished"));
    let result = try_commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("uncertain"));
            Ok(())
        })
    })
    .await;
    assert!(result.is_err());
    assert!(matches!(
        harness
            .session
            .snapshot(
                live(),
                DocOwner::Conversation(conversation),
                &BACKGROUND_CONTEXT
            )
            .await,
        Err(SessionError::Failed { .. })
    ));
    assert!(matches!(
        try_commit(&harness.session, |_tx| Box::pin(async { Ok(()) })).await,
        Err(SessionError::Failed { .. })
    ));
}

/// session-documents.test.ts:174 suppresses writes and publications for empty batches.
#[tokio::test]
async fn empty_batch_publishes_nothing() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    let commits = harness.storage.commits.lock().unwrap().len();
    let publications = harness.publications.lock().unwrap().len();
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    assert_eq!(harness.storage.commits.lock().unwrap().len(), commits);
    assert_eq!(harness.publications.lock().unwrap().len(), publications);
}

// ─── Group 2: uncommitted state invisible ─────────────────────────────────

/// session-documents.test.ts:459 keeps the previous revision unchanged through
/// Storage settlement.
#[tokio::test]
async fn concurrent_reader_waits_for_publication_during_settlement() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    let gate = harness.storage.hold_commits();
    let session = Arc::clone(&harness.session);
    let handle = tokio::spawn(async move {
        session
            .commit(
                commit_fn(move |tx: &mut rpi_durable::session::Transaction| {
                    Box::pin(async move {
                        let draft = tx
                            .doc(live(), DocOwner::Conversation(conversation), None)
                            .await?;
                        draft.write()?["nested"]["count"] = serde_json::json!(9);
                        Ok(())
                    })
                }),
                &BACKGROUND_CONTEXT,
            )
            .await
    });
    gate.entered().await;
    let reading = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .snapshot(
                    live(),
                    DocOwner::Conversation(conversation),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert!(!reading.is_finished(), "the read passed its write gate");
    gate.release();
    handle.await.unwrap().unwrap();
    let seen = reading.await.unwrap().unwrap().unwrap();
    assert_eq!(seen["nested"]["count"], serde_json::json!(9));
    let after = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after["nested"]["count"], serde_json::json!(9));
}

// ─── Group 3: ReadAfterWrite ──────────────────────────────────────────────

/// session-tables.test.ts:82 allows table reads only before the first table write.
#[tokio::test]
async fn table_read_after_first_write_errors() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            assert!(tx.conversation(conversation).await?.is_some());
            let mut draft = rpi_durable::session::EntryDraft::new("note");
            draft.data = Some(serde_json::json!("one"));
            tx.append_entry(conversation, draft).await?;
            let error = tx.conversation(conversation).await.unwrap_err();
            assert!(matches!(
                error,
                SessionError::Storage(StorageError::ReadAfterWrite("conversation"))
            ));
            // Document access stays legal.
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
}

// ─── Group 4: documents ───────────────────────────────────────────────────

/// session-documents.test.ts:250 memoizes concurrent duplicate acquisition.
#[tokio::test]
async fn memoizes_duplicate_acquisition() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let mints = harness.storage.mint_count.load(Ordering::SeqCst);
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let first = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            let second = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            assert!(Arc::ptr_eq(&first.cell, &second.cell));
            {
                let mut guard = first.write()?;
                guard["message"] = serde_json::json!("once");
            }
            let third = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            assert!(Arc::ptr_eq(&first.cell, &third.cell));
            Ok(())
        })
    })
    .await;
    assert_eq!(harness.storage.mint_count.load(Ordering::SeqCst), mints + 1);
    let creates = harness
        .storage
        .last_commit()
        .unwrap()
        .iter()
        .filter(|write| {
            matches!(
                write,
                rpi_durable::types::StorageWrite::DocumentCreate { .. }
            )
        })
        .count();
    assert_eq!(creates, 1);
}

/// session-documents.test.ts:109 never creates on snapshot and returns undefined.
#[tokio::test]
async fn snapshot_absent_is_undefined() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let commits = harness.storage.commits.lock().unwrap().len();
    assert!(
        harness
            .session
            .snapshot(
                live(),
                DocOwner::Conversation(conversation),
                &BACKGROUND_CONTEXT
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        harness
            .session
            .snapshot(counter(), DocOwner::Session, &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(harness.storage.commits.lock().unwrap().len(), commits);
}

/// session-documents.test.ts:479 retires documents and recreates at the same address.
#[tokio::test]
async fn retires_and_recreates_at_same_address() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft.write()?["message"] = serde_json::json!("final");
            tx.retire_doc(live(), DocOwner::Conversation(conversation))
                .await?;
            let replacement = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            assert!(!Arc::ptr_eq(&draft.cell, &replacement.cell));
            replacement.write()?["message"] = serde_json::json!("new");
            Ok(())
        })
    })
    .await;
    let kinds: Vec<rpi_durable::session::DocumentChangeKind> = harness
        .publications
        .lock()
        .expect("publications")
        .iter()
        .flat_map(|publication| publication.changes.iter())
        .filter_map(|change| match change {
            rpi_durable::session::CommitChange::Document(document) => Some(document.kind),
            _ => None,
        })
        .collect();
    assert!(kinds.contains(&rpi_durable::session::DocumentChangeKind::Change));
    assert!(kinds.contains(&rpi_durable::session::DocumentChangeKind::Retire));
    let writes = harness.storage.last_commit().unwrap();
    assert!(writes.iter().any(|write| matches!(
        write,
        rpi_durable::types::StorageWrite::DocumentRetire { .. }
    )));
    assert!(writes.iter().any(|write| matches!(
        write,
        rpi_durable::types::StorageWrite::DocumentCreate { .. }
    )));
    let snapshot = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["message"], serde_json::json!("new"));
    // Retiring after recreation leaves the address absent.
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.retire_doc(live(), DocOwner::Conversation(conversation))
                .await
        })
    })
    .await;
    assert!(
        harness
            .session
            .snapshot(
                live(),
                DocOwner::Conversation(conversation),
                &BACKGROUND_CONTEXT
            )
            .await
            .unwrap()
            .is_none()
    );
}

/// session-checkpoints-migrations.test.ts:376 writes a required migration base
/// then deltas.
#[tokio::test]
async fn migration_writes_base_then_delta() {
    static OLD: OnceLock<DocToken> = OnceLock::new();
    static CURRENT: OnceLock<DocToken> = OnceLock::new();
    let old = OLD.get_or_init(|| {
        session_doc("test.migration", 1, |_| {
            object(serde_json::json!({"count": 4}))
        })
    });
    let current = CURRENT.get_or_init(|| {
        let mut definition = DocDefinition {
            kind: "test.migration".to_string(),
            version: 3,
            scope: DocScope::Session,
            history: None,
            fork: None,
            family: false,
            initial: Arc::new(|_| object(serde_json::json!({"count": 0}))),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, _revision| false),
        };
        definition.migrate = Some(Arc::new(|value, from| {
            let count = value.get("count").and_then(JsonValue::as_u64).unwrap_or(0);
            serde_json::json!({"count": count + from - 1})
        }));
        define_doc(definition)
    });
    let harness = open_test_session();
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(old, DocOwner::Session, None).await?;
            Ok(())
        })
    })
    .await;
    harness.session.unload_documents().await.unwrap();
    assert_eq!(
        harness
            .session
            .snapshot(current, DocOwner::Session, &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .unwrap(),
        object(serde_json::json!({"count": 4}))
    );
    // First successful transaction writes the required migration base.
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(current, DocOwner::Session, None).await?;
            Ok(())
        })
    })
    .await;
    let writes = harness.storage.last_commit().unwrap();
    let content = writes
        .iter()
        .find_map(|write| match write {
            rpi_durable::types::StorageWrite::DocumentChange { content, .. } => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(content.is_base());
    assert_eq!(content.version(), 3);
    // Later edits write deltas.
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx.doc(current, DocOwner::Session, None).await?;
            draft.write()?["count"] = serde_json::json!(7);
            Ok(())
        })
    })
    .await;
    let writes = harness.storage.last_commit().unwrap();
    let content = writes
        .iter()
        .find_map(|write| match write {
            rpi_durable::types::StorageWrite::DocumentChange { content, .. } => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(!content.is_base());
}

/// session-checkpoints-migrations.test.ts:42 selects a base when the predicate
/// returns true for a nonempty delta batch.
#[tokio::test]
async fn checkpoint_predicate_upgrades_delta_to_base() {
    static TOKEN: OnceLock<DocToken> = OnceLock::new();
    let token = TOKEN.get_or_init(|| {
        let mut definition = DocDefinition {
            kind: "test.checkpoint".to_string(),
            version: 1,
            scope: DocScope::Session,
            history: None,
            fork: None,
            family: false,
            initial: Arc::new(|_| object(serde_json::json!({"count": 0}))),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, _revision| false),
        };
        definition.checkpoint_when = Arc::new(|_value, _ops, _deltas_since_base| true);
        define_doc(definition)
    });
    let harness = open_test_session();
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(token, DocOwner::Session, None).await?;
            Ok(())
        })
    })
    .await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx.doc(token, DocOwner::Session, None).await?;
            draft.write()?["count"] = serde_json::json!(1);
            Ok(())
        })
    })
    .await;
    let content = harness
        .storage
        .last_commit()
        .unwrap()
        .iter()
        .find_map(|write| match write {
            rpi_durable::types::StorageWrite::DocumentChange { content, .. } => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(
        content.is_base(),
        "predicate must upgrade the delta to a base"
    );
}

/// session-documents.test.ts:207 revokes escaped drafts when the callback settles.
#[tokio::test]
async fn draft_is_revoked_after_settle() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let escaped = Arc::new(Mutex::new(None));
    let escaped_for_commit = Arc::clone(&escaped);
    commit(&harness.session, move |tx| {
        let escaped = Arc::clone(&escaped_for_commit);
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft.write()?["message"] = serde_json::json!("inside");
            *escaped.lock().unwrap() = Some(draft.clone());
            Ok(())
        })
    })
    .await;
    let draft = escaped.lock().unwrap().clone().unwrap();
    assert!(
        draft.read().is_err(),
        "escaped draft must be revoked after settle"
    );
    assert!(draft.write().is_err());
}

// ─── Group 5: forks ───────────────────────────────────────────────────────

/// session-forks.test.ts:30 copies as-of and current bases and leaves initial absent.
#[tokio::test]
async fn fork_copies_asof_current_and_leaves_initial() {
    static ASOF: OnceLock<DocToken> = OnceLock::new();
    static CURRENT: OnceLock<DocToken> = OnceLock::new();
    static INITIAL: OnceLock<DocToken> = OnceLock::new();
    let as_of = ASOF.get_or_init(|| {
        conversation_doc(
            "fork.asof",
            1,
            DocumentHistory::Rewindable,
            DocumentFork::AsOf,
            |_| object(serde_json::json!({"value": "as-initial"})),
        )
    });
    let current = CURRENT.get_or_init(|| {
        conversation_doc(
            "fork.current",
            1,
            DocumentHistory::Latest,
            DocumentFork::Current,
            |_| object(serde_json::json!({"value": "current-initial"})),
        )
    });
    let initial = INITIAL.get_or_init(|| {
        conversation_doc(
            "fork.initial",
            1,
            DocumentHistory::Latest,
            DocumentFork::Initial,
            |_| object(serde_json::json!({"value": "fresh"})),
        )
    });
    let harness = open_test_session();
    let parent = create_conversation(&harness.session).await;
    let fork_at = commit(&harness.session, move |tx| {
        Box::pin(async move {
            let entry = tx
                .append_entry(parent, rpi_durable::session::EntryDraft::new("fork-point"))
                .await?;
            tx.doc(as_of, DocOwner::Conversation(parent), None)
                .await?
                .write()?["value"] = serde_json::json!("as-at-fork");
            tx.doc(current, DocOwner::Conversation(parent), None)
                .await?
                .write()?["value"] = serde_json::json!("current-at-fork");
            tx.doc(initial, DocOwner::Conversation(parent), None)
                .await?
                .write()?["value"] = serde_json::json!("parent-only");
            Ok(entry.id)
        })
    })
    .await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(as_of, DocOwner::Conversation(parent), None)
                .await?
                .write()?["value"] = serde_json::json!("as-after-fork");
            tx.doc(current, DocOwner::Conversation(parent), None)
                .await?
                .write()?["value"] = serde_json::json!("current-when-copied");
            Ok(())
        })
    })
    .await;
    let child = commit(&harness.session, move |tx| {
        Box::pin(async move {
            Ok(tx
                .fork_conversation(
                    parent,
                    fork_at,
                    rpi_durable::session::ConversationOwnership::Ownerless,
                )
                .await?
                .id)
        })
    })
    .await;
    assert_eq!(
        harness
            .session
            .snapshot(as_of, DocOwner::Conversation(child), &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .unwrap()["value"],
        serde_json::json!("as-at-fork")
    );
    assert_eq!(
        harness
            .session
            .snapshot(current, DocOwner::Conversation(child), &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .unwrap()["value"],
        serde_json::json!("current-when-copied")
    );
    assert!(
        harness
            .session
            .snapshot(initial, DocOwner::Conversation(child), &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
    let copies = document_copy_changes(harness.publications.lock().unwrap().last().unwrap());
    assert_eq!(copies.len(), 2);
}

/// session-forks.test.ts:152 uses final document state from the fork entry
/// commit while excluding later same-commit entries.
#[tokio::test]
async fn fork_excludes_later_same_commit_entries() {
    static DOC: OnceLock<DocToken> = OnceLock::new();
    let doc = DOC.get_or_init(|| {
        conversation_doc(
            "fork.same-commit",
            1,
            DocumentHistory::Rewindable,
            DocumentFork::AsOf,
            |_| object(serde_json::json!({"value": "initial"})),
        )
    });
    let harness = open_test_session();
    let parent = create_conversation(&harness.session).await;
    let (fork_at, excluded) = commit(&harness.session, move |tx| {
        Box::pin(async move {
            let included = tx
                .append_entry(parent, rpi_durable::session::EntryDraft::new("included"))
                .await?;
            tx.doc(doc, DocOwner::Conversation(parent), None)
                .await?
                .write()?["value"] = serde_json::json!("final-state-of-commit");
            let excluded = tx
                .append_entry(parent, rpi_durable::session::EntryDraft::new("excluded"))
                .await?;
            Ok((included.id, excluded.id))
        })
    })
    .await;
    let child = commit(&harness.session, move |tx| {
        Box::pin(async move {
            Ok(tx
                .fork_conversation(
                    parent,
                    fork_at,
                    rpi_durable::session::ConversationOwnership::Ownerless,
                )
                .await?
                .id)
        })
    })
    .await;
    assert_eq!(
        harness
            .session
            .snapshot(doc, DocOwner::Conversation(child), &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .unwrap()["value"],
        serde_json::json!("final-state-of-commit")
    );
    let visible = commit(&harness.session, move |tx| {
        Box::pin(async move {
            Ok(tx
                .scan_entries(
                    rpi_durable::types::EntryQuery {
                        conversation_id: child,
                        min_entry_id: None,
                        max_entry_id: None,
                        order: None,
                    },
                    10,
                    None,
                )
                .await?
                .items
                .into_iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>())
        })
    })
    .await;
    assert!(visible.contains(&fork_at));
    assert!(!visible.contains(&excluded));
}

/// session-forks.test.ts:450 rejects current and as-of source writes.
#[tokio::test]
async fn fork_rejects_source_writes() {
    static CURRENT: OnceLock<DocToken> = OnceLock::new();
    let current = CURRENT.get_or_init(|| {
        conversation_doc(
            "fork.source.current",
            1,
            DocumentHistory::Latest,
            DocumentFork::Current,
            |_| object(serde_json::json!({"value": "committed"})),
        )
    });
    let harness = open_test_session();
    let parent = create_conversation(&harness.session).await;
    let fork_at = commit(&harness.session, move |tx| {
        Box::pin(async move {
            let entry = tx
                .append_entry(parent, rpi_durable::session::EntryDraft::new("point"))
                .await?;
            tx.doc(current, DocOwner::Conversation(parent), None)
                .await?;
            Ok(entry.id)
        })
    })
    .await;
    let result = try_commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(current, DocOwner::Conversation(parent), None)
                .await?
                .write()?["value"] = serde_json::json!("before-fork");
            tx.fork_conversation(
                parent,
                fork_at,
                rpi_durable::session::ConversationOwnership::Ownerless,
            )
            .await?;
            Ok(())
        })
    })
    .await;
    assert!(result.is_err());
}

/// session-forks.test.ts:664 copies every family member across storage scan pages.
#[tokio::test]
async fn fork_copies_every_family_member_across_pages() {
    static FAMILY: OnceLock<DocToken> = OnceLock::new();
    let family = FAMILY.get_or_init(|| {
        conversation_family(
            "fork.pagination",
            1,
            DocumentHistory::Latest,
            DocumentFork::Current,
            |seed| {
                let value = seed.and_then(JsonValue::as_u64).unwrap_or(0);
                object(serde_json::json!({"value": value}))
            },
        )
    });
    let harness = open_test_session();
    let parent = create_conversation(&harness.session).await;
    let fork_at = commit(&harness.session, move |tx| {
        Box::pin(async move {
            Ok(tx
                .append_entry(parent, rpi_durable::session::EntryDraft::new("point"))
                .await?
                .id)
        })
    })
    .await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            for index in 0..260u64 {
                let key = format!("member-{index}");
                tx.doc(
                    family,
                    DocOwner::ConversationFamily(parent, &key),
                    Some(serde_json::json!(index)),
                )
                .await?;
            }
            Ok(())
        })
    })
    .await;
    let _child = commit(&harness.session, move |tx| {
        Box::pin(async move {
            Ok(tx
                .fork_conversation(
                    parent,
                    fork_at,
                    rpi_durable::session::ConversationOwnership::Ownerless,
                )
                .await?
                .id)
        })
    })
    .await;
    let copies = document_copy_changes(harness.publications.lock().unwrap().last().unwrap());
    assert_eq!(copies.len(), 260);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_commits_group_admitted_batches_and_publish_in_line_order() {
    let harness = open_test_session();
    let gate = harness.storage.hold_commits();
    let next_admission = Arc::new(AtomicU64::new(0));
    let mut jobs = Vec::new();
    for index in 0..8u64 {
        let session = Arc::clone(&harness.session);
        let next_admission = Arc::clone(&next_admission);
        jobs.push(tokio::spawn(async move {
            session
                .commit(
                    commit_fn(move |tx| {
                        let key = format!("group-{index}");
                        let next_admission = Arc::clone(&next_admission);
                        Box::pin(async move {
                            let admission = next_admission.fetch_add(1, Ordering::SeqCst);
                            tx.doc(
                                session_family(),
                                DocOwner::Family(&key),
                                Some(serde_json::json!(admission)),
                            )
                            .await?;
                            Ok(admission)
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while next_admission.load(Ordering::SeqCst) != 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all callbacks must start while storage is in flight");
    let admitted = Arc::new(tokio::sync::Notify::new());
    let settling = tokio::spawn({
        let session = Arc::clone(&harness.session);
        let admitted = Arc::clone(&admitted);
        async move {
            session
                .read_on_line(move || {
                    admitted.notify_one();
                    Box::pin(async { Ok(()) })
                })
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), admitted.notified())
        .await
        .expect("independent commits must admit while storage is in flight");
    gate.release();
    for job in jobs {
        job.await.expect("join").expect("commit");
    }
    settling.await.expect("join").expect("settle line");
    let calls = harness.storage.commit_calls.lock().expect("calls");
    assert_eq!(calls.iter().map(Vec::len).sum::<usize>(), 8);
    assert!(
        calls.iter().any(|batches| batches.len() > 1),
        "Session admitted no multi-batch Storage::commit: {calls:?}"
    );
    drop(calls);
    let published: Vec<u64> = harness
        .publications
        .lock()
        .expect("publications")
        .iter()
        .flat_map(session_support::document_changes)
        .map(|change| {
            change.value.expect("created value")["value"]
                .as_u64()
                .expect("admission value")
        })
        .collect();
    assert_eq!(published, (0..8).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outcomes_are_fenced_in_line_order_without_losing_their_own_results() {
    let harness = open_test_session();
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let gate = harness.storage.hold_commits();
    let first = tokio::spawn({
        let session = Arc::clone(&harness.session);
        let delivered = Arc::clone(&delivered);
        async move {
            let result = session
                .commit(
                    commit_fn(|tx| {
                        Box::pin(async move {
                            tx.doc(counter(), DocOwner::Session, None).await.map(|_| ())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await;
            delivered.lock().unwrap().push("first");
            result
        }
    });
    gate.entered().await;
    let read_started = Arc::new(tokio::sync::Notify::new());
    let read = tokio::spawn({
        let session = Arc::clone(&harness.session);
        let delivered = Arc::clone(&delivered);
        let read_started = Arc::clone(&read_started);
        async move {
            let result = session
                .read_on_line(move || {
                    read_started.notify_one();
                    Box::pin(async { Ok("read") })
                })
                .await;
            delivered.lock().unwrap().push("read");
            result
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), read_started.notified())
        .await
        .unwrap();
    let none_started = Arc::new(tokio::sync::Notify::new());
    let none = tokio::spawn({
        let session = Arc::clone(&harness.session);
        let delivered = Arc::clone(&delivered);
        let none_started = Arc::clone(&none_started);
        async move {
            let result = session
                .commit(
                    commit_fn(move |_tx| {
                        none_started.notify_one();
                        Box::pin(async { Ok("none") })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await;
            delivered.lock().unwrap().push("none");
            result
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), none_started.notified())
        .await
        .unwrap();
    let error_started = Arc::new(tokio::sync::Notify::new());
    let error = tokio::spawn({
        let session = Arc::clone(&harness.session);
        let delivered = Arc::clone(&delivered);
        let error_started = Arc::clone(&error_started);
        async move {
            let result = session
                .commit(
                    commit_fn(move |_tx| {
                        error_started.notify_one();
                        Box::pin(async { Err::<(), _>(SessionError::message("own")) })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await;
            delivered.lock().unwrap().push("error");
            result
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), error_started.notified())
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(delivered.lock().unwrap().is_empty());
    gate.release();
    first.await.unwrap().unwrap();
    assert_eq!(read.await.unwrap().unwrap(), "read");
    assert_eq!(none.await.unwrap().unwrap(), "none");
    assert!(
        matches!(error.await.unwrap(), Err(SessionError::Message(message)) if message == "own")
    );
    assert_eq!(delivered.lock().unwrap().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_line_job_does_not_stall_later_outcomes() {
    let harness = open_test_session();
    let gate = harness.storage.hold_commits();
    let first = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .commit(
                    commit_fn(|tx| {
                        Box::pin(async move {
                            tx.doc(counter(), DocOwner::Session, None).await.map(|_| ())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    gate.entered().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let second = tokio::spawn({
        let session = Arc::clone(&harness.session);
        let started = Arc::clone(&started);
        async move {
            session
                .commit(
                    commit_fn(move |_tx| {
                        started.notify_one();
                        Box::pin(async { Ok(()) })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    second.abort();
    let _ = second.await;
    let third = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move { session.read_on_line(|| Box::pin(async { Ok(()) })).await }
    });
    gate.release();
    first.await.unwrap().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), third)
        .await
        .expect("the cancelled job must not strand the fence")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintenance_storage_failure_does_not_poison_an_earlier_admitted_success() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_live_deferred(&harness.session, conversation, "a", Some(0.0)).await;
    write_live_deferred(&harness.session, conversation, "held", Some(20.0)).await;
    let gate = harness.storage.hold_commits();
    let current = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .commit(
                    commit_fn(move |tx| {
                        Box::pin(async move {
                            tx.append_entry(conversation, EntryDraft::new("successful"))
                                .await?;
                            Ok(())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    gate.entered().await;
    let fired = Arc::new(AtomicBool::new(false));
    let storage = Arc::clone(&harness.storage);
    let _listener = harness.session.subscribe_commits(Arc::new(move |_, _| {
        if !fired.swap(true, Ordering::SeqCst) {
            storage
                .fail_next_commit_after_receipt(StorageError::message("deadline storage failure"));
        }
    }));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    gate.release();
    current
        .await
        .expect("the earlier successful commit must keep its own outcome")
        .expect("successful commit");
    let end = tokio::time::timeout(std::time::Duration::from_secs(2), harness.session.closed())
        .await
        .expect("maintenance failure must close the Session");
    assert!(matches!(end, SessionEnd::Failed { error } if error == "deadline storage failure"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successful_maintenance_does_not_wait_for_caller_delivery() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_live_deferred(&harness.session, conversation, "base", Some(0.0)).await;
    write_live_deferred(&harness.session, conversation, "held", Some(60_000.0)).await;
    let gate = harness.storage.hold_commits();
    let session = Arc::clone(&harness.session);
    let earlier = session.commit(
        commit_fn(|tx| {
            Box::pin(async move {
                tx.doc(counter(), DocOwner::Session, None).await?;
                Ok(())
            })
        }),
        &BACKGROUND_CONTEXT,
    );
    tokio::pin!(earlier);
    let entered = gate.entered();
    tokio::pin!(entered);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if futures::poll!(entered.as_mut()).is_ready() {
                break;
            }
            assert!(futures::poll!(earlier.as_mut()).is_pending());
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("earlier admission");
    gate.release();
    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        harness.session.unload_documents(),
    )
    .await
    .expect("maintenance must not wait for an unrelated caller delivery")
    .unwrap();
    earlier.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_maintenance_flush_does_not_leave_a_fence_hole() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_live_deferred(&harness.session, conversation, "base", Some(0.0)).await;
    write_live_deferred(&harness.session, conversation, "held", Some(60_000.0)).await;
    let gate = harness.storage.hold_commits();
    let unload = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move { session.unload_documents().await }
    });
    gate.entered().await;
    unload.abort();
    let _ = unload.await;
    gate.release();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        harness.session.read_on_line(|| Box::pin(async { Ok(()) })),
    )
    .await
    .expect("dropped maintenance must settle its line-index fence")
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outcomes_after_an_earlier_storage_failure_are_session_failed() {
    let harness = open_test_session();
    harness
        .storage
        .fail_next_commit(StorageError::message("first storage failure"));
    let gate = harness.storage.hold_commits();
    let first = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .commit(
                    commit_fn(|tx| {
                        Box::pin(async move {
                            tx.doc(counter(), DocOwner::Session, None).await.map(|_| ())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    gate.entered().await;
    let read = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move { session.read_on_line(|| Box::pin(async { Ok(()) })).await }
    });
    let none = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .commit(
                    commit_fn(|_tx| Box::pin(async { Ok(()) })),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    let error = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .commit(
                    commit_fn(|_tx| Box::pin(async { Err::<(), _>(SessionError::message("own")) })),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !first.is_finished() && !read.is_finished() && !none.is_finished() && !error.is_finished()
    );
    gate.release();
    assert!(matches!(
        first.await.unwrap(),
        Err(SessionError::Storage(_))
    ));
    for result in [
        read.await.unwrap(),
        none.await.unwrap(),
        error.await.unwrap(),
    ] {
        assert!(
            matches!(result, Err(SessionError::Failed { cause }) if cause == "first storage failure")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_part_gate_opens_only_after_the_session_has_failed() {
    let harness = open_test_session();
    let gate = harness.storage.hold_commits();
    let blocker = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .commit(
                    commit_fn(|tx| {
                        Box::pin(async move {
                            tx.doc(counter(), DocOwner::Session, None).await.map(|_| ())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    gate.entered().await;
    let next_admission = Arc::new(AtomicU64::new(0));
    let mut jobs = Vec::new();
    for _ in 0..128 {
        jobs.push(tokio::spawn({
            let session = Arc::clone(&harness.session);
            let next_admission = Arc::clone(&next_admission);
            async move {
                let admission = next_admission.fetch_add(1, Ordering::SeqCst);
                let result = session
                    .commit(
                        commit_fn(move |tx| {
                            let key = format!("gate-{admission}");
                            Box::pin(async move {
                                tx.doc(
                                    session_family(),
                                    DocOwner::Family(&key),
                                    Some(serde_json::json!(admission)),
                                )
                                .await?;
                                Ok(())
                            })
                        }),
                        &BACKGROUND_CONTEXT,
                    )
                    .await;
                (admission, result)
            }
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while next_admission.load(Ordering::SeqCst) != 128 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let admitted = Arc::new(tokio::sync::Notify::new());
    let settling = tokio::spawn({
        let session = Arc::clone(&harness.session);
        let admitted = Arc::clone(&admitted);
        async move {
            session
                .read_on_line(move || {
                    admitted.notify_one();
                    Box::pin(async { Ok(()) })
                })
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), admitted.notified())
        .await
        .unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    let storage = Arc::clone(&harness.storage);
    let _listener = harness.session.subscribe_commits(Arc::new(move |_, _| {
        if !fired.swap(true, Ordering::SeqCst) {
            storage.fail_next_commit_after_receipt(StorageError::message("io after receipt"));
        }
    }));
    let reader = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .storage()
                .find_document(
                    DocumentAddress {
                        kind: session_family().definition.kind.clone(),
                        scope: DocumentScope::Session,
                        key: Some("gate-0".to_string()),
                    },
                    DocumentPoint::CURRENT,
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    gate.release();
    let read = tokio::time::timeout(std::time::Duration::from_secs(2), reader)
        .await
        .expect("reader must not remain gated")
        .unwrap();
    assert!(matches!(read, Err(StorageError::SessionFailed(cause)) if cause == "io after receipt"));
    blocker.await.unwrap().unwrap();
    assert!(matches!(
        settling.await.unwrap(),
        Err(SessionError::Failed { .. })
    ));
    for job in jobs {
        let (_, result) = job.await.unwrap();
        assert!(result.is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reader_waiting_for_an_unpublished_key_does_not_block_writer_publication() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let gate = harness.storage.hold_commits();
    let first = {
        let session = Arc::clone(&harness.session);
        tokio::spawn(async move {
            session
                .commit(
                    commit_fn(|tx| {
                        Box::pin(async move {
                            tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                                serde_json::json!(1);
                            Ok(())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        })
    };
    gate.entered().await;
    let second_started = Arc::new(tokio::sync::Notify::new());
    let second = {
        let session = Arc::clone(&harness.session);
        let second_started = Arc::clone(&second_started);
        tokio::spawn(async move {
            session
                .commit(
                    commit_fn(move |tx| {
                        second_started.notify_one();
                        Box::pin(async move {
                            let draft = tx.doc(counter(), DocOwner::Session, None).await?;
                            let previous = draft.read()?["count"].as_u64().expect("previous count");
                            draft.write()?["count"] = serde_json::json!(previous + 1);
                            Ok(previous)
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), second_started.notified())
        .await
        .expect("the second callback must reach the read gate");
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    gate.release();
    tokio::time::timeout(std::time::Duration::from_secs(2), first)
        .await
        .expect("first commit deadlocked")
        .expect("join")
        .expect("first commit");
    let previous = tokio::time::timeout(std::time::Duration::from_secs(2), second)
        .await
        .expect("read gate and publication deadlocked")
        .expect("join")
        .expect("second commit");
    assert_eq!(previous, 1);
    let value = harness
        .session
        .snapshot(counter(), DocOwner::Session, &BACKGROUND_CONTEXT)
        .await
        .expect("snapshot")
        .expect("counter");
    assert_eq!(value["count"], serde_json::json!(2));
}

#[tokio::test]
async fn commit_outcomes_are_bounded_to_256() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let gate = harness.storage.hold_commits();
    let mut jobs = Vec::new();
    for value in 0..300u64 {
        let session = Arc::clone(&harness.session);
        jobs.push(tokio::spawn(async move {
            session
                .commit(
                    commit_fn(move |tx| {
                        Box::pin(async move {
                            tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                                serde_json::json!(value);
                            Ok(())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }));
    }
    let mut reached = false;
    for _ in 0..200 {
        if harness.session.outstanding_commit_count() == 256 {
            reached = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(reached, "admission did not reach the 256-outcome limit");
    assert!(!jobs[299].is_finished(), "work past the limit was admitted");
    gate.release();
    for job in jobs {
        job.await.expect("join").expect("commit");
    }
    assert_eq!(harness.session.outstanding_commit_count(), 0);
}

// ─── Group 6: watches ─────────────────────────────────────────────────────

type DeliveryLog = Arc<Mutex<Vec<(Option<JsonObject>, Vec<Op>)>>>;

fn delivery_log() -> DeliveryLog {
    Arc::new(Mutex::new(Vec::new()))
}

/// D03: an idle listener receives each committed frame in order.
#[tokio::test]
async fn watch_delivers_unfolded_frames_in_order() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let watch = harness
        .session
        .watch_doc(counter(), DocOwner::Session, &BACKGROUND_CONTEXT)
        .await
        .unwrap()
        .unwrap();
    let log = delivery_log();
    let log_for_listener = Arc::clone(&log);
    watch
        .start(watch_listener(move |call, _context| {
            let log = Arc::clone(&log_for_listener);
            Box::pin(async move {
                if let WatchCall::Value { value, ops, .. } = call {
                    log.lock().unwrap().push((value, ops));
                }
                Ok(())
            })
        }))
        .expect("start");
    for value in 1..=2 {
        commit(&harness.session, move |tx| {
            Box::pin(async move {
                tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                    serde_json::json!(value);
                Ok(())
            })
        })
        .await;
        flush().await;
    }
    let delivered = log.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    assert_eq!(
        delivered[0].0.as_ref().unwrap()["count"],
        serde_json::json!(1)
    );
    assert_eq!(
        delivered[1].0.as_ref().unwrap()["count"],
        serde_json::json!(2)
    );
    assert!(!delivered[0].1.is_empty());
    assert_eq!(watch.stop().await, WatchEnd::Stopped);
}

/// Past 10,000 pending operations, only one root replacement remains.
///
/// This end-to-end case performs 10,001 session commits (about 7.9 GiB peak
/// RSS and over two minutes in debug builds), so the default workspace gate
/// skips it; `chord::delta::tests::folds_busy_leaf_operations_and_overflows_to_replacement`
/// covers the same 10,001-op collapse directly. Run the explicit gate with:
///
/// ```sh
/// cargo test -p rpi-durable --test session_kernel -- --ignored watch_overflow
/// ```
#[ignore = "10,001 commits; run with --ignored for the end-to-end overflow gate"]
#[tokio::test]
async fn watch_overflow_collapses_to_root_replacement() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let watch = harness
        .session
        .watch_doc(counter(), DocOwner::Session, &BACKGROUND_CONTEXT)
        .await
        .unwrap()
        .unwrap();
    for value in 1..=rpi_durable::chord::delta::MAX_PENDING_OPS as u64 + 1 {
        commit(&harness.session, move |tx| {
            Box::pin(async move {
                tx.doc(counter(), DocOwner::Session, None).await?.write()?
                    [format!("count{value}")] = serde_json::json!(value);
                Ok(())
            })
        })
        .await;
    }
    let log = delivery_log();
    let log_for_listener = Arc::clone(&log);
    watch
        .start(watch_listener(move |call, _context| {
            let log = Arc::clone(&log_for_listener);
            Box::pin(async move {
                if let WatchCall::Value { value, ops, .. } = call {
                    log.lock().unwrap().push((value, ops));
                }
                Ok(())
            })
        }))
        .expect("start");
    flush().await;
    let delivered = log.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0]
            .0
            .as_ref()
            .unwrap()
            .get(&format!(
                "count{}",
                rpi_durable::chord::delta::MAX_PENDING_OPS + 1
            ))
            .and_then(JsonValue::as_u64),
        Some((rpi_durable::chord::delta::MAX_PENDING_OPS + 1) as u64)
    );
    assert!(matches!(delivered[0].1.as_slice(), [Op::Replace(_)]));
    let _ = watch.stop().await;
}

/// Retirement is a value call; recreation at the same address keeps the watch alive.
#[tokio::test]
async fn watch_retirement_continues_through_recreation() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let watch = harness
        .session
        .watch_doc(counter(), DocOwner::Session, &BACKGROUND_CONTEXT)
        .await
        .unwrap()
        .unwrap();
    let log = delivery_log();
    let log_for_listener = Arc::clone(&log);
    watch
        .start(watch_listener(move |call, _context| {
            let log = Arc::clone(&log_for_listener);
            Box::pin(async move {
                if let WatchCall::Value { value, ops, .. } = call {
                    log.lock().unwrap().push((value, ops));
                }
                Ok(())
            })
        }))
        .expect("start");
    harness
        .session
        .set_document_retention(Arc::new(|| Some(0.0)));
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.retire_doc(counter(), DocOwner::Session).await })
    })
    .await;
    flush().await;
    settle_line(&harness.session).await;
    let retired_sizes = harness.session.document_cache_sizes();
    assert_eq!(retired_sizes.cached, 0);
    assert_eq!(retired_sizes.attached, 0);
    assert_eq!(log.lock().unwrap().len(), 1);
    assert!(watch.value().is_some());
    assert!(!watch.is_closed());
    commit(&harness.session, |tx| {
        Box::pin(async move {
            tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                serde_json::json!(7);
            Ok(())
        })
    })
    .await;
    flush().await;
    settle_line(&harness.session).await;
    let recreated_sizes = harness.session.document_cache_sizes();
    assert_eq!(recreated_sizes.cached, 1);
    assert_eq!(recreated_sizes.attached, 1);
    let delivered = log.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    assert_eq!(
        delivered[1].0.as_ref().unwrap()["count"],
        serde_json::json!(7)
    );
    // The recreation is a root replacement with the new value, not an
    // empty-ops delta relative to the old incarnation.
    assert!(
        matches!(delivered[1].1.as_slice(), [Op::Replace(value)] if value["count"] == serde_json::json!(7)),
        "recreation ops: {:?}",
        delivered[1].1
    );
    watch.close_session(None);
    assert_eq!(watch.closed().await, WatchEnd::Ended);
    settle_line(&harness.session).await;
    let stopped_sizes = harness.session.document_cache_sizes();
    assert_eq!(stopped_sizes.cached, 0);
    assert_eq!(stopped_sizes.attached, 0);
}

#[tokio::test]
async fn failed_close_drops_deferred_writes() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_live_deferred(&harness.session, conversation, "a", Some(0.0)).await;
    write_live_deferred(&harness.session, conversation, "held", None).await;
    assert_eq!(harness.session.deferred_write_count(), 1);
    harness.session.fail("forced failure");
    harness.session.close(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(harness.session.deferred_write_count(), 0);
}

/// session-watches.test.ts:315 Session close is terminal.
#[tokio::test]
async fn watch_close_is_terminal() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let watch = harness
        .session
        .watch_doc(counter(), DocOwner::Session, &BACKGROUND_CONTEXT)
        .await
        .unwrap()
        .unwrap();
    harness.session.close(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(watch.closed().await, WatchEnd::Ended);
}

/// A watch that attaches while close already sealed admission must reject, as
/// upstream `subscribeClose()`'s `assertUsable()` does, instead of returning a
/// watch whose `closed()` never resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_doc_racing_close_rejects_instead_of_hanging() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let holding = tokio::spawn({
        let session = Arc::clone(&harness.session);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        async move {
            session
                .commit(
                    commit_fn(move |_tx| {
                        let entered = Arc::clone(&entered);
                        let release = Arc::clone(&release);
                        Box::pin(async move {
                            entered.notify_one();
                            release.notified().await;
                            Ok(())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the line is held");
    let watch = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .watch_doc(counter(), DocOwner::Session, &BACKGROUND_CONTEXT)
                .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let closing = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move { session.close(&BACKGROUND_CONTEXT).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    release.notify_one();
    holding.await.unwrap().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), watch)
        .await
        .expect("the watch must not hang")
        .unwrap();
    assert!(matches!(
        result,
        Err(SessionError::Closed) | Err(SessionError::Failed { .. })
    ));
    closing.await.unwrap().unwrap();
}

#[tokio::test]
async fn conversation_view_ignores_custom_conversation_documents() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let views = ConversationViews::new(Arc::clone(&harness.session));
    let watch = views
        .watch(
            conversation,
            ConversationWindow::default(),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch");
    let custom = conversation_doc(
        "custom.view",
        1,
        DocumentHistory::Latest,
        DocumentFork::Initial,
        |_| object(serde_json::json!({"x": 0})),
    );
    let custom = custom.clone();
    commit(&harness.session, move |tx| {
        let custom = custom.clone();
        Box::pin(async move {
            tx.doc(&custom, DocOwner::Conversation(conversation), None)
                .await?
                .write()?["x"] = serde_json::json!(1);
            Ok(())
        })
    })
    .await;
    flush().await;
    let view = watch.value().expect("view");
    assert!(
        !view.docs.contains_key("custom.view"),
        "custom conversation doc leaked into the mounted view: {:?}",
        view.docs.keys().collect::<Vec<_>>()
    );
    watch.stop().await;
}

#[tokio::test]
async fn conversation_view_survives_mounted_doc_recreation() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let views = ConversationViews::new(Arc::clone(&harness.session));
    let watch = views
        .watch(
            conversation,
            ConversationWindow::default(),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch");
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&delivered);
    watch
        .start(Arc::new(move |call, _context| {
            if let WatchCall::Value { value, .. } = call {
                sink.lock().unwrap().push(value);
            }
            Box::pin(async { Ok::<(), String>(()) })
        }))
        .expect("start");
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                .await?;
            drop(draft.write()?);
            Ok(())
        })
    })
    .await;
    flush().await;
    assert!(
        delivered
            .lock()
            .unwrap()
            .last()
            .expect("created view")
            .docs
            .contains_key("pi.live")
    );
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.retire_doc(&LIVE_DOC, DocOwner::Conversation(conversation))
                .await
        })
    })
    .await;
    flush().await;
    assert!(
        !delivered
            .lock()
            .unwrap()
            .last()
            .expect("retired view")
            .docs
            .contains_key("pi.live")
    );
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(&LIVE_DOC, DocOwner::Conversation(conversation), None)
                .await?;
            drop(draft.write()?);
            Ok(())
        })
    })
    .await;
    flush().await;
    assert!(
        delivered
            .lock()
            .unwrap()
            .last()
            .expect("recreated view")
            .docs
            .contains_key("pi.live"),
        "recreated mounted doc must replace, not disappear"
    );
    watch.stop().await;
}

#[tokio::test]
async fn deferred_holds_are_pinned_under_zero_retention() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?
                .write()?["message"] = serde_json::json!("a");
            Ok(())
        })
    })
    .await;
    harness
        .session
        .set_document_retention(Arc::new(|| Some(0.0)));
    let deferred = |value: &'static str| {
        let session = Arc::clone(&harness.session);
        async move {
            session
                .commit_with_options(
                    commit_fn(move |tx| {
                        Box::pin(async move {
                            tx.doc(live(), DocOwner::Conversation(conversation), None)
                                .await?
                                .write()?["message"] = serde_json::json!(value);
                            Ok(())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                    None,
                    CommitOptions {
                        persistence: Some(CommitPersistence::Deferred {
                            within_ms: Some(1000.0),
                        }),
                    },
                )
                .await
                .expect("deferred");
        }
    };
    deferred("ab").await;
    deferred("abc").await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.append_entry(conversation, EntryDraft::new("flush"))
                .await?;
            Ok(())
        })
    })
    .await;
    let value = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("document");
    assert_eq!(value["message"], serde_json::json!("abc"));
}

struct RecordingCommitObserver {
    publications: Arc<Mutex<Vec<CommitPublication>>>,
    ends: Arc<Mutex<Vec<WatchEnd>>>,
}

impl CommitObserver for RecordingCommitObserver {
    fn advance(
        &self,
        publication: &CommitPublication,
        _context: &rpi_durable::chord::context::Context,
    ) {
        self.publications.lock().unwrap().push(publication.clone());
    }

    fn end(&self, end: WatchEnd) {
        self.ends.lock().unwrap().push(end);
    }
}

#[allow(clippy::type_complexity)]
fn commit_observer() -> (
    Arc<RecordingCommitObserver>,
    Arc<Mutex<Vec<CommitPublication>>>,
    Arc<Mutex<Vec<WatchEnd>>>,
) {
    let publications = Arc::new(Mutex::new(Vec::new()));
    let ends = Arc::new(Mutex::new(Vec::new()));
    (
        Arc::new(RecordingCommitObserver {
            publications: Arc::clone(&publications),
            ends: Arc::clone(&ends),
        }),
        publications,
        ends,
    )
}

#[tokio::test]
async fn observe_commits_publishes_only_stored_deferred_changes() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?
                .write()?["message"] = serde_json::json!("one");
            Ok(())
        })
    })
    .await;
    let (observer, publications, _ends) = commit_observer();
    let observation = harness.session.observe_commits(observer);
    harness
        .session
        .commit_with_options(
            commit_fn(move |tx| {
                Box::pin(async move {
                    let draft = tx
                        .doc(live(), DocOwner::Conversation(conversation), None)
                        .await?;
                    draft.write()?["message"] = serde_json::json!("two");
                    Ok(())
                })
            }),
            &BACKGROUND_CONTEXT,
            None,
            CommitOptions {
                persistence: Some(CommitPersistence::Deferred {
                    within_ms: Some(1000.0),
                }),
            },
        )
        .await
        .expect("deferred commit");
    // The first commit above was immediate; the second one is deferred.
    assert_eq!(publications.lock().unwrap().len(), 0);
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.append_entry(conversation, EntryDraft::new("flush"))
                .await?;
            Ok(())
        })
    })
    .await;
    let published = publications.lock().unwrap().clone();
    assert_eq!(
        published.len(),
        1,
        "the unrelated entry is the only stored publication"
    );
    assert!(
        published[0].changes.iter().all(|change| !matches!(
            change,
            rpi_durable::session::CommitChange::Document(document)
                if document.record.kind == "test.live"
        )),
        "an unrelated write must leave the deferred change held: {:?}",
        published[0].changes
    );
    observation.detach.unsubscribe();
}

#[tokio::test]
async fn adopt_publishes_before_a_storage_failure_after_receipt() {
    let harness = open_test_session();
    let (observer, publications, _ends) = commit_observer();
    let observation = harness.session.observe_commits(observer);
    harness
        .storage
        .fail_next_commit_after_receipt(StorageError::message("I/O after receipt"));
    let result = try_commit(&harness.session, |tx| {
        Box::pin(async move {
            tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                serde_json::json!(1);
            Ok(())
        })
    })
    .await;
    assert!(result.is_err(), "the storage failure must fail the commit");
    assert_eq!(
        publications.lock().unwrap().len(),
        1,
        "adoption publishes before the failure"
    );
    assert!(harness.session.failure().is_some());
    observation.detach.unsubscribe();
}

#[tokio::test]
async fn observe_commits_ends_when_the_session_closes() {
    let harness = open_test_session();
    let (observer, _publications, ends) = commit_observer();
    let observation = harness.session.observe_commits(observer);
    harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("close");
    flush().await;
    assert_eq!(ends.lock().unwrap().as_slice(), [WatchEnd::Ended]);
    observation.detach.unsubscribe();
}

#[tokio::test]
async fn observe_commits_after_close_ends_immediately() {
    let harness = open_test_session();
    harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("close");
    let (observer, _publications, ends) = commit_observer();
    let observation = harness.session.observe_commits(observer);
    assert_eq!(ends.lock().unwrap().as_slice(), [WatchEnd::Ended]);
    observation.detach.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observe_commits_racing_close_ends_exactly_once() {
    for _ in 0..64 {
        let harness = open_test_session();
        let (observer, _publications, ends) = commit_observer();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let registering = tokio::spawn({
            let session = Arc::clone(&harness.session);
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                session.observe_commits(observer)
            }
        });
        let closing = tokio::spawn({
            let session = Arc::clone(&harness.session);
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                session.close(&BACKGROUND_CONTEXT).await
            }
        });
        let observation = registering.await.unwrap();
        closing.await.unwrap().unwrap();
        flush().await;
        assert_eq!(ends.lock().unwrap().len(), 1);
        observation.detach.unsubscribe();
    }
}

/// Repeated close returns the same storage failure without retrying the backend.
#[tokio::test]
async fn close_repeats_the_same_storage_failure_without_retrying() {
    let harness = open_test_session();
    harness
        .storage
        .fail_next_close(StorageError::message("close failed"));
    let first = harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect_err("first close must surface the storage error");
    let second = harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect_err("second close returns the same failure");
    assert!(matches!(&first, SessionError::Storage(_)));
    assert!(matches!(&second, SessionError::Storage(_)));
    assert_eq!(first.to_string(), "close failed");
    assert_eq!(second.to_string(), first.to_string());
    assert_eq!(harness.storage.close_count.load(Ordering::SeqCst), 1);
}

/// session-watches.test.ts:353 cancellation is terminal.
#[tokio::test]
async fn watch_cancellation_is_terminal() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let (child, cancel) = rpi_durable::chord::context::with_cancel(&BACKGROUND_CONTEXT);
    let watch = harness
        .session
        .watch_doc(counter(), DocOwner::Session, &child)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    assert_eq!(watch.closed().await, WatchEnd::Aborted);
}

/// D03: callbacks do not overlap, and busy frames coalesce to the latest value.
#[tokio::test]
async fn watch_serializes_and_coalesces_busy_delivery() {
    let harness = open_test_session();
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.doc(counter(), DocOwner::Session, None).await.map(|_| ()) })
    })
    .await;
    let watch = harness
        .session
        .watch_doc(counter(), DocOwner::Session, &BACKGROUND_CONTEXT)
        .await
        .unwrap()
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let values: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let active = Arc::new(AtomicU64::new(0));
    let max_active = Arc::new(AtomicU64::new(0));
    let (entered_l, release_l, values_l) = (
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&values),
    );
    let (active_l, max_l) = (Arc::clone(&active), Arc::clone(&max_active));
    watch
        .start(watch_listener(move |call, _context| {
            let value = match call {
                WatchCall::Value { value, .. } => value,
                WatchCall::Failed { .. } => None,
            };
            let entered = Arc::clone(&entered_l);
            let release = Arc::clone(&release_l);
            let values = Arc::clone(&values_l);
            let active = Arc::clone(&active_l);
            let max_active = Arc::clone(&max_l);
            Box::pin(async move {
                active.fetch_add(1, Ordering::SeqCst);
                let count = values.lock().unwrap().len();
                values.lock().unwrap().push(
                    value
                        .as_ref()
                        .and_then(|value| value["count"].as_u64())
                        .unwrap_or(u64::MAX),
                );
                if count == 0 {
                    entered.notify_one();
                    release.notified().await;
                }
                max_active.fetch_max(active.load(Ordering::SeqCst), Ordering::SeqCst);
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
        }))
        .expect("start");
    commit(&harness.session, |tx| {
        Box::pin(async move {
            tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                serde_json::json!(1);
            Ok(())
        })
    })
    .await;
    entered.notified().await;
    for value in 2..=5u64 {
        commit(&harness.session, move |tx| {
            Box::pin(async move {
                tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                    serde_json::json!(value);
                Ok(())
            })
        })
        .await;
    }
    release.notify_one();
    flush().await;
    let delivered = values.lock().unwrap().clone();
    assert_eq!(delivered, vec![1, 5]);
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "callbacks must not overlap"
    );
    let _ = watch.stop().await;
}

/// session-tables.test.ts:405 retires task documents at terminal settlement,
/// including documents created in the same transaction.
#[tokio::test]
async fn terminal_task_retires_task_documents() {
    static PROGRESS: OnceLock<DocToken> = OnceLock::new();
    static STEP: OnceLock<DocToken> = OnceLock::new();
    let progress = PROGRESS.get_or_init(|| {
        task_doc("test.progress", 1, |_| {
            object(serde_json::json!({"lines": []}))
        })
    });
    let step = STEP.get_or_init(|| {
        define_doc_family(DocDefinition {
            kind: "test.step".to_string(),
            version: 1,
            scope: DocScope::Task,
            history: None,
            fork: None,
            family: true,
            initial: Arc::new(|_| object(serde_json::json!({"lines": []}))),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, _revision| false),
        })
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let task_id = commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.create_task(rpi_durable::session::TaskCreate {
                kind: "test.work".to_string(),
                version: 1,
                input: serde_json::json!({"path": "a"}),
                checkpoint: serde_json::json!({"phase": "start"}),
                ownership: rpi_durable::session::TaskOwnership::Conversation(conversation),
                conversation_id: None,
                background: false,
                abandon_on_restart: false,
                trace: Default::default(),
                links: Vec::new(),
            })
            .await
        })
    })
    .await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(progress, DocOwner::Task(task_id), None).await?;
            tx.doc(
                step,
                DocOwner::TaskFamily(task_id, "committed"),
                Some(serde_json::json!(null)),
            )
            .await?;
            Ok(())
        })
    })
    .await;
    harness.session.unload_documents().await.unwrap();
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            // Create one more task document, then terminate the task in the same transaction.
            tx.doc(
                step,
                DocOwner::TaskFamily(task_id, "new"),
                Some(serde_json::json!(null)),
            )
            .await?;
            let mut task = tx.task(task_id).await?.expect("task");
            task.state = rpi_durable::types::TaskState::Terminal {
                outcome: rpi_durable::types::TaskOutcome::Completed {
                    result: serde_json::json!({"ok": true}),
                },
            };
            tx.set_task(task)?;
            Ok(())
        })
    })
    .await;
    let writes = harness.storage.last_commit().unwrap();
    let retirements = writes
        .iter()
        .filter(|write| {
            matches!(
                write,
                rpi_durable::types::StorageWrite::DocumentRetire { .. }
            )
        })
        .count();
    let creations = writes
        .iter()
        .filter(|write| {
            matches!(
                write,
                rpi_durable::types::StorageWrite::DocumentCreate { .. }
            )
        })
        .count();
    assert_eq!(creations, 1);
    assert_eq!(
        retirements, 3,
        "two committed + one same-transaction document"
    );
    assert!(
        harness
            .session
            .snapshot(progress, DocOwner::Task(task_id), &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
}

/// session-documents.test.ts:266 uses the first family seed and ignores later seeds.
#[tokio::test]
async fn family_first_seed_wins() {
    static MEMBER: OnceLock<DocToken> = OnceLock::new();
    let member = MEMBER.get_or_init(|| {
        define_doc_family(DocDefinition {
            kind: "test.member".to_string(),
            version: 1,
            scope: DocScope::Session,
            history: None,
            fork: None,
            family: true,
            initial: Arc::new(|seed| {
                object(serde_json::json!({
                    "seed": seed.cloned().unwrap_or(JsonValue::Null),
                    "hits": 0
                }))
            }),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, _revision| false),
        })
    });
    let harness = open_test_session();
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let first = tx
                .doc(
                    member,
                    DocOwner::Family("k"),
                    Some(serde_json::json!("first")),
                )
                .await?;
            let second = tx
                .doc(
                    member,
                    DocOwner::Family("k"),
                    Some(serde_json::json!("second")),
                )
                .await?;
            assert!(Arc::ptr_eq(&first.cell, &second.cell));
            first.write()?["hits"] = serde_json::json!(1);
            Ok(())
        })
    })
    .await;
    let snapshot = harness
        .session
        .snapshot(member, DocOwner::Family("k"), &BACKGROUND_CONTEXT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["seed"], serde_json::json!("first"));
    assert_eq!(snapshot["hits"], serde_json::json!(1));
}

/// session-watches.test.ts:40 never creates an absent document.
#[tokio::test]
async fn watch_absent_is_undefined() {
    let harness = open_test_session();
    let commits = harness.storage.commits.lock().unwrap().len();
    let missing = rpi_durable::ids::id_from_number::<rpi_durable::ids::Conversation>(999);
    assert!(
        harness
            .session
            .watch_doc(live(), DocOwner::Conversation(missing), &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(harness.storage.commits.lock().unwrap().len(), commits);
}

// ─── Group 7: historical read ─────────────────────────────────────────────

/// session-checkpoints-migrations.test.ts:735 selects the incarnation alive at
/// the entry commit.
#[tokio::test]
async fn snapshot_as_of_reads_history() {
    static DOC: OnceLock<DocToken> = OnceLock::new();
    let doc = DOC.get_or_init(|| {
        conversation_doc(
            "history.incarnation",
            1,
            DocumentHistory::Rewindable,
            DocumentFork::AsOf,
            |_| object(serde_json::json!({"value": "initial"})),
        )
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let before = commit(&harness.session, move |tx| {
        Box::pin(async move {
            Ok(tx
                .append_entry(
                    conversation,
                    rpi_durable::session::EntryDraft::new("before"),
                )
                .await?
                .id)
        })
    })
    .await;
    let created_at: EntryId = commit(&harness.session, move |tx| {
        Box::pin(async move {
            let entry = tx
                .append_entry(
                    conversation,
                    rpi_durable::session::EntryDraft::new("create"),
                )
                .await?;
            tx.doc(doc, DocOwner::Conversation(conversation), None)
                .await?
                .write()?["value"] = serde_json::json!("old");
            Ok(entry.id)
        })
    })
    .await;
    assert!(
        harness
            .session
            .snapshot_as_of(doc, conversation, before, &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        harness
            .session
            .snapshot_as_of(doc, conversation, created_at, &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .unwrap()["value"],
        serde_json::json!("old")
    );
}

// ─── Group 12: storage failures end the Session (V18-06 R-D11) ────────────

/// R-D11: a `StorageRequestError` from a read fails only that call; the Session
/// stays usable.
#[tokio::test]
async fn read_request_error_fails_only_its_call() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    harness
        .storage
        .fail_next_read(StorageError::Request(StorageRequestError::new(
            "invalid cursor",
        )));
    let result = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await;
    assert!(matches!(
        result,
        Err(SessionError::Storage(StorageError::Request(_)))
    ));
    // The Session is still healthy: a commit and a later read succeed.
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("kept"));
            Ok(())
        })
    })
    .await;
    assert!(harness.session.failure().is_none());
}

/// R-D11: a read whose caller was cancelled is the second exempt case: it
/// fails only that call, even when the backend throws.
#[tokio::test]
async fn a_cancelled_read_error_fails_only_its_call() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let gate = harness.storage.hold_find_document();
    harness
        .storage
        .fail_next_read(StorageError::message("cancelled read"));
    let token = CancellationToken::new();
    let context = with_abort_signal(token.clone(), &BACKGROUND_CONTEXT);
    let future = harness
        .session
        .snapshot(live(), DocOwner::Conversation(conversation), &context);
    tokio::pin!(future);
    tokio::select! {
        _ = gate.entered() => {}
        result = &mut future => panic!("read did not reach storage: {result:?}"),
    }
    token.cancel();
    gate.release();
    let result = future.await;
    assert!(matches!(
        result,
        Err(SessionError::Storage(StorageError::Message(_)))
    ));
    assert!(harness.session.failure().is_none());

    let value = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await;
    assert!(value.is_ok(), "a cancelled read must not fail the Session");
}

/// R-D11: a callback that swallows a failing read must not commit or report
/// success; the guard's failure wins.
#[tokio::test]
async fn a_swallowed_read_failure_fails_the_commit() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    harness
        .session
        .unload_documents()
        .await
        .expect("unload documents");
    harness
        .storage
        .fail_next_read(StorageError::message("swallowed read"));
    let commits_before = harness.storage.commits.lock().expect("commits").len();
    let result = harness
        .session
        .commit(
            commit_fn(move |tx| {
                Box::pin(async move {
                    let _ = tx
                        .doc(live(), DocOwner::Conversation(conversation), None)
                        .await;
                    Ok::<(), SessionError>(())
                })
            }),
            &BACKGROUND_CONTEXT,
        )
        .await;
    assert!(
        matches!(result, Err(SessionError::Failed { .. })),
        "swallowed read was accepted: {result:?}"
    );
    assert_eq!(
        harness.storage.commits.lock().expect("commits").len(),
        commits_before,
        "no batch may commit after the guard failed"
    );
}

/// R-D11: any other read error fails the Session; later calls and `closed`
/// report `SessionFailed`.
#[tokio::test]
async fn other_read_error_fails_the_session() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    harness
        .storage
        .fail_next_read(StorageError::message("disk vanished"));
    let result = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await;
    assert!(matches!(
        result,
        Err(SessionError::Storage(StorageError::Message(_)))
    ));
    assert!(matches!(
        harness
            .session
            .snapshot(
                live(),
                DocOwner::Conversation(conversation),
                &BACKGROUND_CONTEXT
            )
            .await,
        Err(SessionError::Failed { .. })
    ));
    assert_eq!(
        harness.session.closed().await,
        SessionEnd::Failed {
            error: "disk vanished".to_string()
        }
    );
}

/// R-D11: once failed, guarded calls do not reach the backend and return the
/// guard's failure marker instead of a successful read.
#[tokio::test]
async fn guarded_reads_reject_after_the_session_fails() {
    let harness = open_test_session();
    harness.session.fail("platform reset");
    let result = harness
        .session
        .storage()
        .conversation(rpi_durable::ids::ROOT_CONVERSATION_ID, &BACKGROUND_CONTEXT)
        .await;
    assert!(matches!(
        result,
        Err(StorageError::SessionFailed(cause)) if cause == "platform reset"
    ));
}

/// R-D11: a read that was underway when another path failed cannot return its
/// now-untrusted value; it ends with `SessionFailed` after the backend settles.
#[tokio::test]
async fn an_underway_read_rejects_when_the_session_fails() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let gate = harness.storage.hold_find_document();
    let session = Arc::clone(&harness.session);
    let reading = tokio::spawn(async move {
        session
            .snapshot(
                live(),
                DocOwner::Conversation(conversation),
                &BACKGROUND_CONTEXT,
            )
            .await
    });
    gate.entered().await;
    harness.session.fail("concurrent failure");
    gate.release();
    assert!(matches!(
        reading.await.expect("join"),
        Err(SessionError::Failed { cause }) if cause == "concurrent failure"
    ));
}

/// R-D11: a commit already admitted by the guard exposes the same SessionFailed
/// shape as later calls when another path fails while storage is underway.
#[tokio::test]
async fn an_underway_commit_rejects_when_the_session_fails() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let gate = harness.storage.hold_commits();
    let session = Arc::clone(&harness.session);
    let committing = tokio::spawn(async move {
        session
            .commit(
                commit_fn(move |tx| {
                    Box::pin(async move {
                        let draft = tx
                            .doc(live(), DocOwner::Conversation(conversation), None)
                            .await?;
                        draft.write()?["nested"]["count"] = serde_json::json!(9);
                        Ok(())
                    })
                }),
                &BACKGROUND_CONTEXT,
            )
            .await
    });
    gate.entered().await;
    harness.session.fail("concurrent failure");
    gate.release();
    assert!(matches!(
        committing.await.expect("join"),
        Err(SessionError::Failed { cause }) if cause == "concurrent failure"
    ));
}

/// A commit future created before a close must not run its callback after
/// the close won the session line, and must not fail the Session.
#[tokio::test]
async fn a_commit_polled_after_close_rejects_without_running() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let ran = Arc::new(AtomicBool::new(false));
    let ran_in_commit = Arc::clone(&ran);
    let pending = harness.session.commit(
        commit_fn(move |tx| {
            let ran = Arc::clone(&ran_in_commit);
            Box::pin(async move {
                ran.store(true, Ordering::SeqCst);
                let draft = tx
                    .doc(live(), DocOwner::Conversation(conversation), None)
                    .await?;
                draft.write()?["nested"]["count"] = serde_json::json!(9);
                Ok(())
            })
        }),
        &BACKGROUND_CONTEXT,
    );
    harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("close");
    let result = pending.await;
    assert!(matches!(result, Err(SessionError::Closed)), "{result:?}");
    assert!(
        !ran.load(Ordering::SeqCst),
        "the commit callback ran after close"
    );
    assert!(harness.session.failure().is_none());
    assert!(matches!(harness.session.closed().await, SessionEnd::Closed));
}

/// The guarded close seals new calls and waits for calls already underway.
#[tokio::test]
async fn guarded_close_waits_for_an_underway_read() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let gate = harness.storage.hold_find_document();
    let session = Arc::clone(&harness.session);
    let reading = tokio::spawn(async move {
        session
            .snapshot(
                live(),
                DocOwner::Conversation(conversation),
                &BACKGROUND_CONTEXT,
            )
            .await
    });
    gate.entered().await;
    let storage = Arc::clone(harness.session.storage());
    let closing = tokio::spawn(async move { storage.close(&BACKGROUND_CONTEXT).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!closing.is_finished(), "close passed the underway read");
    gate.release();
    assert!(reading.await.expect("join").is_ok());
    closing.await.expect("join").expect("close");
}

/// R-D11: `mintId()` storage failure fails the Session, even from a
/// scheduler/transaction commit path.
#[tokio::test]
async fn a_mint_failure_fails_the_session() {
    let harness = open_test_session();
    harness
        .storage
        .fail_next_mint(StorageError::Message("mint failed".into()));
    let result = harness
        .session
        .commit(
            commit_fn(|tx: &mut rpi_durable::session::Transaction| {
                Box::pin(async move {
                    Ok(tx
                        .create_conversation(rpi_durable::session::ConversationOwnership::Ownerless)
                        .await?
                        .id)
                })
            }),
            &BACKGROUND_CONTEXT,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(
        harness.session.closed().await,
        SessionEnd::Failed {
            error: "mint failed".to_string()
        }
    );
}

/// R-D11: `closed` settles only after the Storage has actually closed, so a
/// host may reopen the backend as soon as it reads the terminal state.
#[tokio::test]
async fn a_failed_session_closed_waits_for_storage_close() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let close_gate = harness.storage.hold_close();
    harness
        .storage
        .fail_next_read(StorageError::message("disk vanished"));
    let _ = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(1), close_gate.entered())
        .await
        .expect("the failed Session did not start closing storage");
    let mut closed = Box::pin(harness.session.closed());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut closed)
            .await
            .is_err(),
        "closed settled before storage.close"
    );
    close_gate.release();
    assert_eq!(
        closed.await,
        SessionEnd::Failed {
            error: "disk vanished".to_string()
        }
    );
}

/// R-D11: a normal close ends `Session.closed` with `closed`.
#[tokio::test]
async fn normal_close_reports_closed() {
    let harness = open_test_session();
    harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("close");
    assert_eq!(harness.session.closed().await, SessionEnd::Closed);
}

/// R-D11: a failed Session ends its watch with `session_failed`, not
/// `session_closed`.
#[tokio::test]
async fn failed_session_ends_watch_with_session_failed() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, {
        move |tx| {
            Box::pin(async move {
                let draft = tx
                    .doc(live(), DocOwner::Conversation(conversation), None)
                    .await?;
                draft
                    .write()?
                    .as_object_mut()
                    .unwrap()
                    .insert("message".into(), serde_json::json!("watched"));
                Ok(())
            })
        }
    })
    .await;
    let watch = harness
        .session
        .watch_doc(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch")
        .expect("document");
    harness.session.fail("platform reset");
    assert_eq!(
        watch.closed().await,
        WatchEnd::Failed {
            error: "platform reset".to_string()
        }
    );
}

/// R-D11: a host `subscribeCommits` listener's panic is contained and reported,
/// and does not fail the Session.
#[tokio::test]
async fn host_commit_listener_panic_is_contained() {
    let harness = open_test_session();
    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&reported);
    harness
        .session
        .set_listener_reporter(Arc::new(move |message, _context| {
            sink.lock().expect("reported").push(message);
        }));
    let _subscription = harness.session.subscribe_commits(Arc::new(|_, _| {
        panic!("listener exploded");
    }));
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("ok"));
            Ok(())
        })
    })
    .await;
    assert!(harness.session.failure().is_none());
    assert!(
        reported
            .lock()
            .expect("reported")
            .iter()
            .any(|message| message.contains("commit listener failed")),
        "got {reported:?}"
    );
}

/// R-D11: a `subscribeClose` listener's panic is contained and reported; the
/// close still completes.
#[tokio::test]
async fn host_close_listener_panic_is_contained() {
    let harness = open_test_session();
    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&reported);
    harness
        .session
        .set_listener_reporter(Arc::new(move |message, _context| {
            sink.lock().expect("reported").push(message);
        }));
    let _subscription = harness.session.subscribe_close(Arc::new(|| {
        panic!("close listener exploded");
    }));
    harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("close");
    assert_eq!(harness.session.closed().await, SessionEnd::Closed);
    assert!(
        reported
            .lock()
            .expect("reported")
            .iter()
            .any(|message| message.contains("close listener")),
        "got {reported:?}"
    );
}

/// R-D11: a component commit listener's panic fails the Session, but the
/// commit it observed stands (upstream `session-adoption.test.ts:205-234`).
#[tokio::test]
async fn internal_commit_listener_panic_fails_the_session_after_the_commit_stands() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let _subscription = harness.session.subscribe_internal_commits(Arc::new(|_, _| {
        panic!("component exploded");
    }));
    let committed = try_commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("ok"));
            Ok(())
        })
    })
    .await;
    assert!(
        committed.is_ok(),
        "a component listener throw must not reject the triggering commit: {committed:?}"
    );
    flush().await;
    assert!(
        harness
            .session
            .failure()
            .as_deref()
            .is_some_and(|cause| cause.contains("component exploded")),
        "the Session must fail after the commit: {:?}",
        harness.session.failure()
    );
    assert!(matches!(
        try_commit(&harness.session, |_tx| Box::pin(async { Ok(()) })).await,
        Err(SessionError::Failed { .. })
    ));
}

/// R-D10: `documentRetentionMs` drops an unobserved document after its last
/// use, so the next access loads it from Storage again.
#[tokio::test]
async fn document_retention_reloads_an_unobserved_document() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("kept"));
            Ok(())
        })
    })
    .await;
    harness
        .session
        .set_document_retention(Arc::new(|| Some(0.0)));
    settle_line(&harness.session).await;
    let before = harness.storage.document_read_count.load(Ordering::SeqCst);
    harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("first");
    settle_line(&harness.session).await;
    harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("second");
    settle_line(&harness.session).await;
    harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("third");
    let after = harness.storage.document_read_count.load(Ordering::SeqCst);
    assert!(
        after >= before + 2,
        "each access reloads after retention 0: {before} -> {after}"
    );
}

/// Upstream `documentRetention`: `Infinity` keeps documents, so repeated access
/// never reloads an unobserved one.
#[tokio::test]
async fn document_retention_infinity_keeps_an_unobserved_document() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("kept"));
            Ok(())
        })
    })
    .await;
    harness
        .session
        .set_document_retention(Arc::new(|| Some(f64::INFINITY)));
    harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("first");
    let loaded = harness.storage.document_read_count.load(Ordering::SeqCst);
    for _ in 0..3 {
        harness
            .session
            .snapshot(
                live(),
                DocOwner::Conversation(conversation),
                &BACKGROUND_CONTEXT,
            )
            .await
            .expect("again");
    }
    assert_eq!(
        harness.storage.document_read_count.load(Ordering::SeqCst),
        loaded,
        "Infinity keeps the document cached"
    );
}

/// R-D10: a watch keeps its document cached.
#[tokio::test]
async fn document_retention_keeps_an_observed_document() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("kept"));
            Ok(())
        })
    })
    .await;
    harness
        .session
        .set_document_retention(Arc::new(|| Some(0.0)));
    let watch = harness
        .session
        .watch_doc(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch")
        .expect("document");
    let before = harness.storage.document_read_count.load(Ordering::SeqCst);
    harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot");
    assert_eq!(
        harness.storage.document_read_count.load(Ordering::SeqCst),
        before,
        "an observed document stays cached"
    );
    watch.stop().await;
}

/// A document with two observers stays cached when the first one detaches:
/// only the last release unpins the address (upstream counts attaches).
#[tokio::test]
async fn document_retention_counts_each_observer() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("kept"));
            Ok(())
        })
    })
    .await;
    harness
        .session
        .set_document_retention(Arc::new(|| Some(0.0)));
    let first = harness
        .session
        .watch_doc(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch")
        .expect("document");
    let second = harness
        .session
        .watch_doc(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch")
        .expect("document");
    first.stop().await;
    let before = harness.storage.document_read_count.load(Ordering::SeqCst);
    harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot");
    assert_eq!(
        harness.storage.document_read_count.load(Ordering::SeqCst),
        before,
        "the second observer still pins the document"
    );
    second.stop().await;
    // The last release unpins it, so the line sweep evicts before the next access.
    settle_line(&harness.session).await;
    harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot");
    assert!(
        harness.storage.document_read_count.load(Ordering::SeqCst) > before,
        "the last detach releases the address"
    );
}

fn controlled_retention_session(
    retention: f64,
) -> (
    Arc<rpi_durable::session::SessionImpl>,
    Arc<session_support::ControlledStorage>,
    Arc<AtomicU64>,
) {
    let storage = Arc::new(session_support::ControlledStorage::new());
    let clock = Arc::new(AtomicU64::new(0));
    let now = Arc::clone(&clock);
    let session = rpi_durable::session::create_session(
        Arc::clone(&storage) as Arc<dyn rpi_durable::types::Storage>,
        Some(Arc::new(move || now.load(Ordering::SeqCst) as f64)),
    );
    session.set_document_retention(Arc::new(move || Some(retention)));
    (session, storage, clock)
}

#[tokio::test]
async fn document_retention_never_evicts_a_tracker_inside_its_transaction() {
    let (session, _storage, clock) = controlled_retention_session(0.0);
    let conversation = create_conversation(&session).await;
    commit(&session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!(1));
            Ok(())
        })
    })
    .await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let committing = tokio::spawn({
        let session = Arc::clone(&session);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        async move {
            session
                .commit(
                    commit_fn(move |tx| {
                        let entered = Arc::clone(&entered);
                        let release = Arc::clone(&release);
                        Box::pin(async move {
                            let draft = tx
                                .doc(live(), DocOwner::Conversation(conversation), None)
                                .await?;
                            entered.notify_one();
                            release.notified().await;
                            draft
                                .write()?
                                .as_object_mut()
                                .unwrap()
                                .insert("message".into(), serde_json::json!(11));
                            Ok(())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    entered.notified().await;
    clock.store(1_000_000, Ordering::SeqCst);
    release.notify_waiters();
    committing.await.expect("join").expect("commit");
    settle_line(&session).await;
    let value = session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("document");
    assert_eq!(value["message"], 11);
    commit(&session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!(12));
            Ok(())
        })
    })
    .await;
    settle_line(&session).await;
    let value = session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("document");
    assert_eq!(value["message"], 12);
}

#[tokio::test]
async fn document_retention_evicts_on_its_timer_without_a_commit() {
    let (session, _storage, clock) = controlled_retention_session(20.0);
    let conversation = create_conversation(&session).await;
    commit(&session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("kept"));
            Ok(())
        })
    })
    .await;
    assert!(session.document_cache_sizes().cached > 0);
    clock.store(20, Ordering::SeqCst);
    for _ in 0..200 {
        if session.document_cache_sizes().cached == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(session.document_cache_sizes().cached, 0);
}

#[tokio::test]
async fn document_retention_falls_back_to_commit_sweeps_without_timers() {
    let (session, _storage, clock) = controlled_retention_session(10.0);
    session.set_document_timer(
        Some(Arc::new(|_, _, _| None)),
        Arc::new(|message| panic!("unexpected timer report: {message}")),
    );
    let conversation = create_conversation(&session).await;
    commit(&session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft
                .write()?
                .as_object_mut()
                .unwrap()
                .insert("message".into(), serde_json::json!("kept"));
            Ok(())
        })
    })
    .await;
    assert!(session.document_cache_sizes().cached > 0);
    clock.store(10_000, Ordering::SeqCst);
    commit(&session, |_tx| Box::pin(async { Ok(()) })).await;
    assert_eq!(session.document_cache_sizes().cached, 0);
}

#[tokio::test]
async fn document_retention_coalesces_sweeps_while_a_commit_holds_the_line() {
    let (session, _storage, _clock) = controlled_retention_session(0.0);
    let mut conversations = Vec::new();
    for _ in 0..12 {
        let conversation = create_conversation(&session).await;
        commit(&session, move |tx| {
            Box::pin(async move {
                let draft = tx
                    .doc(live(), DocOwner::Conversation(conversation), None)
                    .await?;
                draft
                    .write()?
                    .as_object_mut()
                    .unwrap()
                    .insert("message".into(), serde_json::json!("kept"));
                Ok(())
            })
        })
        .await;
        conversations.push(conversation);
    }
    settle_line(&session).await;
    assert_eq!(session.document_cache_sizes().cached, 0);
    let before = session.document_cache_sizes().sweeps;
    let loading = Arc::new(conversations);
    commit(&session, move |tx| {
        let conversations = Arc::clone(&loading);
        Box::pin(async move {
            for conversation in conversations.iter().copied() {
                tx.doc(live(), DocOwner::Conversation(conversation), None)
                    .await?;
                tokio::task::yield_now().await;
            }
            Ok(())
        })
    })
    .await;
    settle_line(&session).await;
    let sizes = session.document_cache_sizes();
    assert_eq!(sizes.cached, 0);
    assert!(
        sizes.sweeps - before <= 2,
        "sweeps: {before} -> {}",
        sizes.sweeps
    );
}

#[tokio::test]
async fn document_retention_pins_a_replacement_to_its_live_observer() {
    let (session, _storage, _clock) = controlled_retention_session(0.0);
    let conversation = create_conversation(&session).await;
    commit(&session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    let retired = session
        .watch_doc(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch")
        .expect("document");
    commit(&session, move |tx| {
        Box::pin(async move {
            tx.retire_doc(live(), DocOwner::Conversation(conversation))
                .await
        })
    })
    .await;
    commit(&session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft.write()?["message"] = serde_json::json!("replacement");
            Ok(())
        })
    })
    .await;
    settle_line(&session).await;
    let pinned = session.document_cache_sizes();
    assert_eq!(pinned.cached, 1);
    assert_eq!(pinned.attached, 1);
    let value = session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("replacement");
    assert_eq!(value["message"], "replacement");
    retired.stop().await;
    settle_line(&session).await;
    assert_eq!(session.document_cache_sizes().cached, 0);
    assert_eq!(session.document_cache_sizes().attached, 0);
}

#[tokio::test]
async fn document_retention_keeps_an_observed_incarnation_across_versions() {
    let (session, storage, _clock) = controlled_retention_session(0.0);
    let conversation = create_conversation(&session).await;
    commit(&session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    let observed = session
        .watch_doc(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch")
        .expect("document");
    commit(&session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft.write()?["message"] = serde_json::json!("new version");
            Ok(())
        })
    })
    .await;
    settle_line(&session).await;
    let reads = storage.document_read_count.load(Ordering::SeqCst);
    assert_eq!(session.document_cache_sizes().cached, 1);
    session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot");
    assert_eq!(storage.document_read_count.load(Ordering::SeqCst), reads);
    observed.stop().await;
}

#[tokio::test]
async fn document_retention_touches_documents_after_slow_storage_commit() {
    let (session, storage, clock) = controlled_retention_session(10.0);
    let conversation = create_conversation(&session).await;
    commit(&session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    let gate = storage.hold_commits();
    let committing = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            session
                .commit(
                    commit_fn(move |tx| {
                        Box::pin(async move {
                            let draft = tx
                                .doc(live(), DocOwner::Conversation(conversation), None)
                                .await?;
                            draft.write()?["message"] = serde_json::json!("slow");
                            Ok(())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    gate.entered().await;
    clock.store(100, Ordering::SeqCst);
    gate.release();
    committing.await.expect("join").expect("commit");
    assert!(session.document_cache_sizes().cached > 0);
    let value = session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("document");
    assert_eq!(value["message"], "slow");
}

fn deferred_options(within_ms: Option<f64>) -> CommitOptions {
    CommitOptions {
        persistence: Some(CommitPersistence::Deferred { within_ms }),
    }
}

async fn write_live_deferred(
    session: &Arc<rpi_durable::session::SessionImpl>,
    conversation: rpi_durable::ids::ConversationId,
    message: &'static str,
    within_ms: Option<f64>,
) {
    session
        .commit_with_options(
            commit_fn(move |tx| {
                Box::pin(async move {
                    tx.doc(live(), DocOwner::Conversation(conversation), None)
                        .await?
                        .write()?["message"] = serde_json::json!(message);
                    Ok(())
                })
            }),
            &BACKGROUND_CONTEXT,
            None,
            deferred_options(within_ms),
        )
        .await
        .expect("deferred write");
}

#[tokio::test]
async fn deferred_progress_stays_held_on_read_and_flushes_on_close() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let initial = harness.storage.commits.lock().unwrap().len();
    write_live_deferred(&harness.session, conversation, "a", None).await;
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        initial + 1,
        "initial base is immediate"
    );
    write_live_deferred(&harness.session, conversation, "ab", None).await;
    write_live_deferred(&harness.session, conversation, "abc", None).await;
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        initial + 1,
        "consecutive deltas stay held"
    );
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await
                .map(|_| ())
        })
    })
    .await;
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        initial + 1,
        "a read-only commit leaves the merged delta held"
    );
    write_live_deferred(&harness.session, conversation, "abcd", None).await;
    harness.session.close(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        initial + 2,
        "close flushes the last delta"
    );
}

#[tokio::test]
async fn deferred_base_stays_held_and_covers_later_deltas_until_stored() {
    static TOKEN: OnceLock<DocToken> = OnceLock::new();
    let token = TOKEN.get_or_init(|| {
        define_doc(DocDefinition {
            kind: "test.deferred.base".to_string(),
            version: 1,
            scope: DocScope::Conversation,
            history: Some(DocumentHistory::Latest),
            fork: Some(DocumentFork::Initial),
            family: false,
            initial: Arc::new(|_| object(serde_json::json!({"message": "a"}))),
            migrate: None,
            checkpoint_when: Arc::new(|value, _ops, _revision| value["message"] == "base"),
        })
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(token, DocOwner::Conversation(conversation), None)
                .await
                .map(|_| ())
        })
    })
    .await;
    write_deferred_message(&harness.session, token, conversation, "base", None).await;
    assert_eq!(harness.storage.commits.lock().unwrap().len(), 2);
    write_deferred_message(&harness.session, token, conversation, "base-plus", None).await;
    assert_eq!(harness.storage.commits.lock().unwrap().len(), 2);
    write_deferred_message(&harness.session, token, conversation, "final", Some(0.0)).await;
    let calls = harness.storage.commit_calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls.last().unwrap().len(), 1);
    assert!(calls.last().unwrap()[0].iter().any(|write| matches!(
        write,
        rpi_durable::types::StorageWrite::DocumentChange {
            content: rpi_durable::types::DocumentContent::Base { value, .. },
            ..
        } if value["message"] == "final"
    )));
}

#[tokio::test]
async fn deferred_adopt_preserves_stored_delta_metadata_for_checkpoint() {
    static TOKEN: OnceLock<DocToken> = OnceLock::new();
    let token = TOKEN.get_or_init(|| {
        define_doc(DocDefinition {
            kind: "test.deferred.revision".to_string(),
            version: 1,
            scope: DocScope::Conversation,
            history: Some(DocumentHistory::Latest),
            fork: Some(DocumentFork::Initial),
            family: false,
            initial: Arc::new(|_| object(serde_json::json!({"message": "a"}))),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, revision| revision == 1),
        })
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_deferred_message(&harness.session, token, conversation, "a", Some(0.0)).await;
    write_deferred_message(&harness.session, token, conversation, "b", Some(0.0)).await;
    write_deferred_message(&harness.session, token, conversation, "held", None).await;
    write_deferred_message(&harness.session, token, conversation, "final", Some(0.0)).await;
    let calls = harness.storage.commit_calls.lock().unwrap();
    assert!(calls.last().unwrap()[0].iter().any(|write| matches!(
        write,
        rpi_durable::types::StorageWrite::DocumentChange {
            content: rpi_durable::types::DocumentContent::Base { value, .. },
            ..
        } if value["message"] == "final"
    )));
}

#[tokio::test]
async fn deferred_deadline_flush_advances_the_stored_delta_count() {
    static TOKEN: OnceLock<DocToken> = OnceLock::new();
    let token = TOKEN.get_or_init(|| {
        define_doc(DocDefinition {
            kind: "test.deferred.deadline-metadata".to_string(),
            version: 1,
            scope: DocScope::Conversation,
            history: Some(DocumentHistory::Latest),
            fork: Some(DocumentFork::Initial),
            family: false,
            initial: Arc::new(|_| object(serde_json::json!({"message": "a"}))),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, revision| revision >= 1),
        })
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    // The create stores a base; the held delta is stored by the deadline
    // timer, not by a commit that reads or writes the document.
    write_deferred_message(&harness.session, token, conversation, "a", Some(0.0)).await;
    write_deferred_message(&harness.session, token, conversation, "held", Some(10.0)).await;
    let before = harness.storage.commit_calls.lock().unwrap().len();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if harness.storage.commit_calls.lock().unwrap().len() > before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the deadline flush stores the held delta");
    // The maintenance flush must advance `deltasSinceBase`, so this commit's
    // checkpoint predicate sees revision 1 and stores a base (upstream
    // `#storeHolds`).
    write_deferred_message(&harness.session, token, conversation, "after", Some(0.0)).await;
    let calls = harness.storage.commit_calls.lock().unwrap();
    assert!(calls.last().unwrap()[0].iter().any(|write| matches!(
        write,
        rpi_durable::types::StorageWrite::DocumentChange {
            content: rpi_durable::types::DocumentContent::Base { value, .. },
            ..
        } if value["message"] == "after"
    )));
}

#[tokio::test]
async fn deferred_overflow_stores_a_base_and_does_not_accumulate_operations() {
    static TOKEN: OnceLock<DocToken> = OnceLock::new();
    let token = TOKEN.get_or_init(|| {
        conversation_doc(
            "test.deferred.overflow",
            1,
            DocumentHistory::Latest,
            DocumentFork::Initial,
            |_| object(serde_json::json!({"message": "a"})),
        )
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(token, DocOwner::Conversation(conversation), None)
                .await
                .map(|_| ())
        })
    })
    .await;
    let held = token;
    harness
        .session
        .commit_with_options(
            commit_fn(move |tx| {
                Box::pin(async move {
                    let document = tx
                        .doc(held, DocOwner::Conversation(conversation), None)
                        .await?;
                    let mut value = document.write()?;
                    for index in 0..=rpi_durable::chord::delta::MAX_PENDING_OPS as u64 {
                        value[format!("k{index}")] = serde_json::json!(index);
                    }
                    Ok(())
                })
            }),
            &BACKGROUND_CONTEXT,
            None,
            deferred_options(Some(60_000.0)),
        )
        .await
        .expect("overflowing held commit");
    // A held commit after the overflow must be covered by the replacement, not
    // appended to the held operations.
    write_deferred_message(
        &harness.session,
        token,
        conversation,
        "final",
        Some(60_000.0),
    )
    .await;
    let (observer, publications, _ends) = commit_observer();
    let observation = harness.session.observe_commits(observer);
    harness.session.close(&BACKGROUND_CONTEXT).await.unwrap();
    let published = publications.lock().unwrap().clone();
    let change = published
        .iter()
        .flat_map(|publication| &publication.changes)
        .find_map(|change| match change {
            rpi_durable::session::CommitChange::Document(document)
                if document.record.kind == "test.deferred.overflow" =>
            {
                Some(document)
            }
            _ => None,
        })
        .expect("the stored held change is published");
    assert_eq!(change.ops.len(), 1, "overflow stores a replacement");
    assert!(matches!(
        change.ops[0],
        rpi_durable::chord::delta::Op::Replace(_)
    ));
    observation.detach.unsubscribe();
    let calls = harness.storage.commit_calls.lock().unwrap();
    let last = calls.last().unwrap();
    let base = last[0]
        .iter()
        .find_map(|write| match write {
            rpi_durable::types::StorageWrite::DocumentChange {
                content: rpi_durable::types::DocumentContent::Base { value, .. },
                ..
            } => Some(value),
            _ => None,
        })
        .expect("the overflow is stored as a base");
    assert_eq!(base["message"], serde_json::json!("final"));
    assert_eq!(base["k10000"], serde_json::json!(10000));
}

#[tokio::test]
async fn deferred_write_folds_held_and_current_operations_into_one_stored_commit() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_live_deferred(&harness.session, conversation, "a", Some(0.0)).await;
    write_live_deferred(&harness.session, conversation, "ab", None).await;
    let before = harness.storage.commit_calls.lock().unwrap().len();
    write_live_deferred(&harness.session, conversation, "abc", Some(0.0)).await;
    let calls = harness.storage.commit_calls.lock().unwrap();
    assert_eq!(calls.len(), before + 1);
    let call = calls.last().unwrap();
    assert_eq!(call.len(), 1, "held and current writes share one batch");
    let document_writes = call[0]
        .iter()
        .filter(|write| {
            matches!(
                write,
                rpi_durable::types::StorageWrite::DocumentChange { .. }
            )
        })
        .count();
    assert_eq!(document_writes, 1);
    let ops = call[0]
        .iter()
        .find_map(|write| match write {
            rpi_durable::types::StorageWrite::DocumentChange {
                content: rpi_durable::types::DocumentContent::Delta { ops, .. },
                ..
            } => Some(ops.clone()),
            _ => None,
        })
        .expect("folded delta");
    let mut value = serde_json::json!({"message": "a", "items": [], "nested": {"count": 0}});
    rpi_durable::chord::delta::apply(&mut value, &ops).expect("replay folded operations");
    assert_eq!(value["message"], serde_json::json!("abc"));
}

#[tokio::test]
async fn deferred_read_with_other_writes_stores_one_batch_and_one_publication() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_live_deferred(&harness.session, conversation, "a", Some(0.0)).await;
    write_live_deferred(&harness.session, conversation, "held", None).await;
    let (observer, publications, _ends) = commit_observer();
    let observation = harness.session.observe_commits(observer);
    let before = harness.storage.commit_calls.lock().unwrap().len();
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let text = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?
                .read()?["message"]
                .as_str()
                .expect("message")
                .to_string();
            let mut entry = EntryDraft::new("test.derived");
            entry.data = Some(serde_json::json!(text));
            tx.append_entry(conversation, entry).await?;
            Ok(())
        })
    })
    .await;
    let calls = harness.storage.commit_calls.lock().unwrap();
    assert_eq!(calls.len(), before + 1);
    let call = calls.last().unwrap();
    assert_eq!(call.len(), 1);
    assert_eq!(call[0].len(), 2, "document.change + entry");
    assert!(call[0].iter().any(|write| matches!(
        write,
        rpi_durable::types::StorageWrite::DocumentChange { .. }
    )));
    assert!(
        call[0]
            .iter()
            .any(|write| matches!(write, rpi_durable::types::StorageWrite::Entry { .. }))
    );
    let published = publications.lock().unwrap().clone();
    assert_eq!(published.len(), 1);
    assert!(
        published[0].changes.iter().any(|change| matches!(
            change,
            rpi_durable::session::CommitChange::Document(document)
                if document.record.kind == "test.live" && !document.ops.is_empty()
        )),
        "publication: {:?}",
        published[0].changes
    );
    observation.detach.unsubscribe();
}

#[tokio::test]
async fn deferred_rejects_negative_and_nan_within_ms() {
    for within_ms in [-1.0, f64::NAN] {
        let harness = open_test_session();
        let conversation = create_conversation(&harness.session).await;
        let result = harness
            .session
            .commit_with_options(
                commit_fn(move |tx| {
                    Box::pin(async move {
                        tx.doc(live(), DocOwner::Conversation(conversation), None)
                            .await?
                            .write()?["message"] = serde_json::json!("invalid");
                        Ok(())
                    })
                }),
                &BACKGROUND_CONTEXT,
                None,
                CommitOptions {
                    persistence: Some(CommitPersistence::Deferred {
                        within_ms: Some(within_ms),
                    }),
                },
            )
            .await;
        assert!(matches!(result, Err(SessionError::Message(_))));
        assert!(harness.session.failure().is_none());
        assert_eq!(harness.storage.commit_calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn deferred_deadline_zero_and_retirement_flush_at_the_right_boundaries() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_live_deferred(&harness.session, conversation, "a", Some(0.0)).await;
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        2,
        "withinMs=0 is immediate"
    );
    write_live_deferred(&harness.session, conversation, "ab", Some(10.0)).await;
    assert_eq!(harness.storage.commits.lock().unwrap().len(), 2);
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        3,
        "deadline flushes"
    );
    write_live_deferred(&harness.session, conversation, "abc", None).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.retire_doc(live(), DocOwner::Conversation(conversation))
                .await
        })
    })
    .await;
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        4,
        "held delta and retirement persist in one commit"
    );
}

#[tokio::test]
async fn deferred_mixed_within_ms_merges_append_ops_and_uses_the_earliest_deadline() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    // Materialize the document with an empty string first so both deferred
    // writes below are holdable and diff to `Op::Append`.
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            let draft = tx
                .doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            draft.write()?["message"] = serde_json::json!("");
            Ok(())
        })
    })
    .await;
    let initial = harness.storage.commits.lock().unwrap().len();
    write_live_deferred(&harness.session, conversation, "a", Some(1_000.0)).await;
    write_live_deferred(&harness.session, conversation, "ab", Some(20.0)).await;
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        initial,
        "both commits stay held"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while harness.storage.commits.lock().unwrap().len() == initial {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the earlier within_ms deadline must flush");
    assert_eq!(
        harness.storage.commits.lock().unwrap().len(),
        initial + 1,
        "the merged document is stored once"
    );
    let stored_ops = {
        let commits = harness.storage.commits.lock().unwrap();
        let batch = commits.last().expect("stored batch");
        batch
            .iter()
            .find_map(|write| match write {
                rpi_durable::types::StorageWrite::DocumentChange {
                    content: rpi_durable::types::DocumentContent::Delta { ops, .. },
                    ..
                } => Some(ops.clone()),
                _ => None,
            })
            .expect("stored document delta")
    };
    assert!(
        stored_ops.iter().any(|op| matches!(op, Op::Append { .. })),
        "the merged deferred write must carry Append ops: {stored_ops:?}"
    );
    let value = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("document");
    assert_eq!(value["message"], serde_json::json!("ab"));
}

async fn write_deferred_message(
    session: &Arc<rpi_durable::session::SessionImpl>,
    token: &DocToken,
    conversation: rpi_durable::ids::ConversationId,
    message: &'static str,
    within_ms: Option<f64>,
) {
    let token = token.clone();
    session
        .commit_with_options(
            commit_fn(move |tx| {
                let token = token.clone();
                Box::pin(async move {
                    tx.doc(&token, DocOwner::Conversation(conversation), None)
                        .await?
                        .write()?["message"] = serde_json::json!(message);
                    Ok(())
                })
            }),
            &BACKGROUND_CONTEXT,
            None,
            deferred_options(within_ms),
        )
        .await
        .expect("deferred write");
}

#[tokio::test]
async fn deferred_append_watch_replays_only_each_transactions_operations() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_deferred_message(&harness.session, live(), conversation, "", Some(1000.0)).await;
    let watch = harness
        .session
        .watch_doc(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("watch")
        .expect("document");
    let applied = Arc::new(Mutex::new(JsonValue::Object(object(serde_json::json!({
        "message": "",
        "items": [],
        "nested": {"count": 0}
    })))));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener_applied = Arc::clone(&applied);
    let listener_errors = Arc::clone(&errors);
    watch
        .start(watch_listener(move |call, _context| {
            let applied = Arc::clone(&listener_applied);
            let errors = Arc::clone(&listener_errors);
            Box::pin(async move {
                if let WatchCall::Value { value, ops, .. } = call {
                    let mut applied = applied.lock().expect("applied");
                    if let Err(error) = rpi_durable::chord::delta::apply(&mut applied, &ops) {
                        errors.lock().expect("errors").push(error.to_string());
                        return Ok(());
                    }
                    let expected = JsonValue::Object(value.expect("document value"));
                    if *applied != expected {
                        errors.lock().expect("errors").push(format!(
                            "replayed value {applied:?} does not match publication {expected:?}"
                        ));
                    }
                }
                Ok(())
            })
        }))
        .expect("start");
    write_deferred_message(&harness.session, live(), conversation, "a", Some(1000.0)).await;
    flush().await;
    write_deferred_message(&harness.session, live(), conversation, "ab", None).await;
    flush().await;
    assert!(errors.lock().expect("errors").is_empty());
    assert_eq!(
        applied.lock().expect("applied")["message"],
        serde_json::json!("ab")
    );
    watch.stop().await;
    harness.session.close(&BACKGROUND_CONTEXT).await.unwrap();
}

struct EventCommitObserver {
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl CommitObserver for EventCommitObserver {
    fn advance(
        &self,
        _publication: &CommitPublication,
        _context: &rpi_durable::chord::context::Context,
    ) {
        self.events.lock().expect("events").push("advance");
    }

    fn end(&self, _end: WatchEnd) {
        self.events.lock().expect("events").push("end");
    }
}

#[tokio::test]
async fn close_publishes_final_deferred_changes_before_ending_commit_observers() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_deferred_message(&harness.session, live(), conversation, "a", Some(1000.0)).await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let observation = harness
        .session
        .observe_commits(Arc::new(EventCommitObserver {
            events: Arc::clone(&events),
        }));
    write_deferred_message(&harness.session, live(), conversation, "ab", None).await;
    assert!(events.lock().expect("events").is_empty());
    harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("close");
    assert_eq!(
        events.lock().expect("events").as_slice(),
        ["advance", "end"]
    );
    observation.detach.unsubscribe();
}

/// `unloadDocuments()` is a barrier even without held changes: a commit
/// admitted before it must publish before the cache is cleared, or its
/// adoption re-installs a document after the unload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unload_documents_waits_for_an_in_flight_publication() {
    let harness = open_test_session();
    let gate = harness.storage.hold_commits();
    let committing = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move {
            session
                .commit(
                    commit_fn(|tx| {
                        Box::pin(async move {
                            tx.doc(counter(), DocOwner::Session, None).await.map(|_| ())
                        })
                    }),
                    &BACKGROUND_CONTEXT,
                )
                .await
        }
    });
    gate.entered().await;
    let unloading = tokio::spawn({
        let session = Arc::clone(&harness.session);
        async move { session.unload_documents().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !unloading.is_finished(),
        "unload must wait for the admitted publication"
    );
    gate.release();
    committing.await.unwrap().unwrap();
    unloading.await.unwrap().unwrap();
    assert_eq!(harness.session.document_cache_sizes().cached, 0);
}

#[tokio::test]
async fn unload_documents_stores_held_changes_before_cold_loading() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_deferred_message(&harness.session, live(), conversation, "a", Some(1000.0)).await;
    write_deferred_message(&harness.session, live(), conversation, "held", None).await;
    let stored = harness.storage.commits.lock().expect("commits").len();
    harness.session.unload_documents().await.expect("unload");
    assert_eq!(
        harness.storage.commits.lock().expect("commits").len(),
        stored + 1
    );
    let value = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("document");
    assert_eq!(value["message"], serde_json::json!("held"));
}

#[tokio::test]
async fn version_reload_stores_held_changes_before_dropping_the_old_tracker() {
    static CURRENT: OnceLock<DocToken> = OnceLock::new();
    let old = conversation_doc(
        "test.deferred.migration",
        1,
        DocumentHistory::Latest,
        DocumentFork::Initial,
        |_| object(serde_json::json!({"message": null})),
    );
    let current = CURRENT.get_or_init(|| {
        let mut definition = DocDefinition {
            kind: "test.deferred.migration".to_string(),
            version: 2,
            scope: DocScope::Conversation,
            history: Some(DocumentHistory::Latest),
            fork: Some(DocumentFork::Initial),
            family: false,
            initial: Arc::new(|_| object(serde_json::json!({"message": null}))),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, _revision| false),
        };
        definition.migrate = Some(Arc::new(|value, _from| value.clone()));
        define_doc(definition)
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    write_deferred_message(&harness.session, &old, conversation, "a", Some(1000.0)).await;
    write_deferred_message(&harness.session, &old, conversation, "held", None).await;
    let value = harness
        .session
        .snapshot(
            current,
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("document");
    assert_eq!(value["message"], serde_json::json!("held"));
    let before = harness.storage.commit_calls.lock().unwrap().len();
    harness.session.unload_documents().await.unwrap();
    assert_eq!(harness.storage.commit_calls.lock().unwrap().len(), before);
}

#[tokio::test]
async fn version_reload_preserves_unrelated_held_documents() {
    static CURRENT: OnceLock<DocToken> = OnceLock::new();
    let old = Arc::new(conversation_doc(
        "test.deferred.selective",
        1,
        DocumentHistory::Latest,
        DocumentFork::Initial,
        |_| object(serde_json::json!({"message": null})),
    ));
    let current = CURRENT.get_or_init(|| {
        let mut definition = DocDefinition {
            kind: "test.deferred.selective".to_string(),
            version: 2,
            scope: DocScope::Conversation,
            history: Some(DocumentHistory::Latest),
            fork: Some(DocumentFork::Initial),
            family: false,
            initial: Arc::new(|_| object(serde_json::json!({"message": null}))),
            migrate: None,
            checkpoint_when: Arc::new(|_value, _ops, _revision| false),
        };
        definition.migrate = Some(Arc::new(|value, _from| value.clone()));
        define_doc(definition)
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let old_for_commit = Arc::clone(&old);
    commit(&harness.session, move |tx| {
        let old = Arc::clone(&old_for_commit);
        Box::pin(async move {
            tx.doc(&old, DocOwner::Conversation(conversation), None)
                .await?;
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    write_deferred_message(
        &harness.session,
        &old,
        conversation,
        "held-old",
        Some(60_000.0),
    )
    .await;
    write_deferred_message(
        &harness.session,
        live(),
        conversation,
        "held-live",
        Some(60_000.0),
    )
    .await;
    assert_eq!(harness.session.deferred_write_count(), 2);
    let before = harness.storage.commit_calls.lock().unwrap().len();
    let value = harness
        .session
        .snapshot(
            current,
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("snapshot")
        .expect("document");
    assert_eq!(value["message"], serde_json::json!("held-old"));
    assert_eq!(
        harness.storage.commit_calls.lock().unwrap().len(),
        before + 1
    );
    assert_eq!(harness.session.deferred_write_count(), 1);
    let unrelated = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("unrelated snapshot")
        .expect("document");
    assert_eq!(unrelated["message"], serde_json::json!("held-live"));
    assert_eq!(harness.session.deferred_write_count(), 1);
    harness.session.close(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(harness.session.deferred_write_count(), 0);
}

#[tokio::test]
async fn flushing_merged_deferred_documents_releases_every_retention_pin() {
    static OTHER: OnceLock<DocToken> = OnceLock::new();
    let other = OTHER.get_or_init(|| {
        conversation_doc(
            "test.deferred.other",
            1,
            DocumentHistory::Latest,
            DocumentFork::Initial,
            |_| object(serde_json::json!({"message": null})),
        )
    });
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let other_for_commit = other.clone();
    commit(&harness.session, move |tx| {
        let other = other_for_commit.clone();
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?;
            tx.doc(&other, DocOwner::Conversation(conversation), None)
                .await?;
            Ok(())
        })
    })
    .await;
    harness
        .session
        .set_document_retention(Arc::new(|| Some(0.0)));
    write_deferred_message(&harness.session, live(), conversation, "a", Some(1000.0)).await;
    write_deferred_message(&harness.session, other, conversation, "b", Some(1000.0)).await;
    commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.append_entry(conversation, EntryDraft::new("flush"))
                .await?;
            Ok(())
        })
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while harness.session.document_cache_sizes().cached != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all released documents must be evicted");
}

/// Rust host listeners run synchronously during publication (D-111): one
/// that fails the Session makes the current commit reject. Upstream schedules
/// host listeners as a microtask, so its commit would settle first.
#[tokio::test]
async fn publication_rejects_when_the_session_fails_during_a_host_listener() {
    let harness = open_test_session();
    let weak = Arc::downgrade(&harness.session);
    let _subscription = harness.session.subscribe_commits(Arc::new(move |_, _| {
        if let Some(session) = weak.upgrade() {
            session.fail("publication reset");
        }
    }));
    let result = try_commit(&harness.session, |tx| {
        Box::pin(async move {
            tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                serde_json::json!(1);
            Ok(())
        })
    })
    .await;
    assert!(matches!(
        result,
        Err(SessionError::Failed { cause }) if cause == "publication reset"
    ));
}

#[tokio::test]
async fn rejected_storage_error_wins_over_a_publication_listener_failure() {
    let harness = open_test_session();
    let conversation = create_conversation(&harness.session).await;
    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&reported);
    harness
        .session
        .set_listener_reporter(Arc::new(move |message, _context| {
            sink.lock().expect("reported").push(message);
        }));
    let _subscription = harness.session.subscribe_internal_commits(Arc::new(|_, _| {
        panic!("component exploded");
    }));
    harness
        .storage
        .fail_next_commit_after_receipt(StorageError::message("I/O after receipt"));
    let result = try_commit(&harness.session, move |tx| {
        Box::pin(async move {
            tx.doc(live(), DocOwner::Conversation(conversation), None)
                .await?
                .write()?["message"] = serde_json::json!("stored");
            Ok(())
        })
    })
    .await;
    assert!(matches!(
        result,
        Err(SessionError::Storage(error)) if error.to_string() == "I/O after receipt"
    ));
    assert_eq!(
        harness.session.failure().as_deref(),
        Some("I/O after receipt")
    );
    assert!(
        reported
            .lock()
            .expect("reported")
            .iter()
            .any(|message| message.contains("component exploded"))
    );
}

#[tokio::test]
async fn writer_panic_fails_and_settles_the_commit_instead_of_stranding_it() {
    let harness = open_test_session();
    harness.storage.panic_next_commit("storage exploded");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        try_commit(&harness.session, |tx| {
            Box::pin(async move {
                tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                    serde_json::json!(1);
                Ok(())
            })
        }),
    )
    .await
    .expect("writer outcome must settle");
    assert!(matches!(
        result,
        Err(SessionError::Failed { cause }) if cause == "storage exploded"
    ));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), harness.session.closed())
            .await
            .expect("failed session must close"),
        SessionEnd::Failed {
            error: "storage exploded".to_string(),
        }
    );
}
