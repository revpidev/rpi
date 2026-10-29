//! Session-kernel integration tests (port of the session domain of
//! `packages/durable/test/session-*.test.ts` @ `4a080252f`).
//!
//! Each test cites the upstream case it adapts. Rust asserts value equality
//! where upstream asserts reference identity (V17-14 §7.3).

mod session_support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rpi_durable::chord::context::BACKGROUND_CONTEXT;
use rpi_durable::chord::delta::Op;
use rpi_durable::chord::json::{JsonObject, JsonValue};
use rpi_durable::documents::{DocDefinition, DocScope, DocToken, define_doc, define_doc_family};
use rpi_durable::errors::{StorageError, StorageRequestError};
use rpi_durable::ids::EntryId;
use rpi_durable::session::{
    DocOwner, SessionEnd, SessionError, WatchEnd, commit_fn, watch_listener,
};
use rpi_durable::types::{DocumentFork, DocumentHistory};

use session_support::{
    commit, create_conversation, document_copy_changes, flush, open_test_session, try_commit,
};

fn object(value: JsonValue) -> JsonObject {
    value.as_object().cloned().unwrap_or_default()
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
        checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
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
        checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
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
        checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
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
        checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
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
async fn concurrent_reader_sees_committed_value_during_settlement() {
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
    let seen = harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        seen["nested"]["count"],
        serde_json::json!(0),
        "uncommitted value leaked"
    );
    gate.release();
    handle.await.unwrap().unwrap();
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
            checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
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
            checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
        };
        definition.checkpoint_when = Some(Arc::new(|_value, _ops, _deltas_since_base| true));
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

// ─── Group 6: watches ─────────────────────────────────────────────────────

type DeliveryLog = Arc<Mutex<Vec<(Option<JsonObject>, Vec<Op>)>>>;

fn delivery_log() -> DeliveryLog {
    Arc::new(Mutex::new(Vec::new()))
}

/// session-watches.test.ts:48 delivers exact committed frames, serialized.
#[tokio::test]
async fn watch_delivers_exact_frames_in_order() {
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
        .start(watch_listener(move |value, ops, _context| {
            let log = Arc::clone(&log_for_listener);
            Box::pin(async move {
                log.lock().unwrap().push((value, ops));
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
    }
    flush().await;
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

/// session-watches.test.ts:142 collapses 101 pending commits to one root replacement.
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
    for value in 1..=101 {
        commit(&harness.session, move |tx| {
            Box::pin(async move {
                tx.doc(counter(), DocOwner::Session, None).await?.write()?["count"] =
                    serde_json::json!(value);
                Ok(())
            })
        })
        .await;
    }
    let log = delivery_log();
    let log_for_listener = Arc::clone(&log);
    watch
        .start(watch_listener(move |value, ops, _context| {
            let log = Arc::clone(&log_for_listener);
            Box::pin(async move {
                log.lock().unwrap().push((value, ops));
                Ok(())
            })
        }))
        .expect("start");
    flush().await;
    let delivered = log.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0].0.as_ref().unwrap()["count"],
        serde_json::json!(101)
    );
    assert!(matches!(delivered[0].1.as_slice(), [Op::Replace(_)]));
    let _ = watch.stop().await;
}

/// session-watches.test.ts:279 delivers retirement and does not follow recreation.
#[tokio::test]
async fn watch_retirement_is_terminal() {
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
        .start(watch_listener(move |value, ops, _context| {
            let log = Arc::clone(&log_for_listener);
            Box::pin(async move {
                log.lock().unwrap().push((value, ops));
                Ok(())
            })
        }))
        .expect("start");
    commit(&harness.session, |tx| {
        Box::pin(async move { tx.retire_doc(counter(), DocOwner::Session).await })
    })
    .await;
    assert_eq!(watch.closed().await, WatchEnd::Retired);
    let delivered = log.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].0.is_none());
    assert!(watch.value().is_none());
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
    assert_eq!(watch.closed().await, WatchEnd::SessionClosed);
}

/// A failed Session::close must leave the cleanup flag retryable.
#[tokio::test]
async fn close_retries_after_a_storage_failure() {
    let harness = open_test_session();
    harness
        .storage
        .fail_next_close(StorageError::message("close failed once"));
    let error = harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect_err("first close must surface the storage error");
    assert_eq!(error.to_string(), "close failed once");
    harness
        .session
        .close(&BACKGROUND_CONTEXT)
        .await
        .expect("second close must retry and succeed");
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
    assert_eq!(watch.closed().await, WatchEnd::Cancelled);
}

/// session-watches.test.ts:83 serializes callbacks and buffers exact frames
/// while one is in flight.
#[tokio::test]
async fn watch_serializes_delivery() {
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
        .start(watch_listener(move |value, _ops, _context| {
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
    assert_eq!(delivered, vec![1, 2, 3, 4, 5]);
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
            checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
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
            checkpoint_when: Some(Arc::new(|_value, _ops, _revision| false)),
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
        WatchEnd::SessionFailed {
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
        .set_failure_reporter(Arc::new(move |message| {
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

/// R-D11: a component commit listener's panic fails the Session.
#[tokio::test]
async fn internal_commit_listener_panic_fails_the_session() {
    let harness = open_test_session();
    let _subscription = harness.session.subscribe_internal_commits(Arc::new(|_, _| {
        panic!("component exploded");
    }));
    let conversation = create_conversation(&harness.session).await;
    let _ = try_commit(&harness.session, move |tx| {
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
    harness.session.set_document_retention(Arc::new(|| Some(0)));
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
    harness
        .session
        .snapshot(
            live(),
            DocOwner::Conversation(conversation),
            &BACKGROUND_CONTEXT,
        )
        .await
        .expect("second");
    let after = harness.storage.document_read_count.load(Ordering::SeqCst);
    assert!(
        after >= before + 2,
        "each access reloads after retention 0: {before} -> {after}"
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
    harness.session.set_document_retention(Arc::new(|| Some(0)));
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
