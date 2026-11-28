//! Conversation view windows and accumulated transcript integration.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rpi_durable::chord::context::BACKGROUND_CONTEXT;
use rpi_durable::harness::{
    ConversationTranscript, ConversationView, ConversationViews, ConversationWindow, EntryLoader,
    TranscriptItem,
};
use rpi_durable::ids::{EntryId, id_from_number};
use rpi_durable::session::{
    ConversationOwnership, EntryDraft, Observer, SessionError, Subscription, WatchCall, WatchEnd,
    commit_fn, create_session,
};
use rpi_durable::storage::MemoryStorage;
use rpi_durable::types::{Cursor, EntryQuery, EntryRecord, Page, ScanOrder};

struct RecordingObserver {
    values: Arc<Mutex<Vec<ConversationView>>>,
}

impl Observer<ConversationView> for RecordingObserver {
    fn advance(
        &self,
        call: WatchCall<ConversationView>,
        _context: &rpi_durable::chord::context::Context,
    ) {
        if let WatchCall::Value { value, .. } = call {
            self.values.lock().expect("values").push(value);
        }
    }

    fn end(&self, _end: WatchEnd) {}
}

#[tokio::test]
async fn observers_have_independent_bounded_windows() {
    let session = create_session(Arc::new(MemoryStorage::new()), None);
    let context = BACKGROUND_CONTEXT.clone();
    let conversation = session
        .commit(
            commit_fn(|tx| {
                Box::pin(async move {
                    Ok(tx
                        .create_conversation(ConversationOwnership::Ownerless)
                        .await?
                        .id)
                })
            }),
            &context,
        )
        .await
        .expect("conversation");
    let views = ConversationViews::new(Arc::clone(&session));
    let recording = Arc::new(Mutex::new(Vec::new()));
    let observation = views
        .observe(
            conversation,
            Arc::new(RecordingObserver {
                values: Arc::clone(&recording),
            }),
            ConversationWindow {
                from: None,
                min: Some(2),
                max: Some(3),
            },
            &context,
        )
        .await
        .expect("observe");
    for index in 0..6 {
        session
            .commit(
                commit_fn(move |tx| {
                    Box::pin(async move {
                        tx.append_entry(conversation, EntryDraft::new(format!("entry-{index}")))
                            .await?;
                        Ok(())
                    })
                }),
                &context,
            )
            .await
            .expect("append");
    }
    let values = recording.lock().expect("values").clone();
    assert_eq!(observation.value.entries.len(), 0);
    assert_eq!(values.last().expect("latest").entries.len(), 2);
    let expected_before = values.last().expect("latest").entries[0].id.get() - 1;
    assert_eq!(
        values.last().expect("latest").before,
        Some(id_from_number(expected_before))
    );
    let watch = views
        .watch(
            conversation,
            ConversationWindow {
                from: None,
                min: Some(1),
                max: Some(100),
            },
            &context,
        )
        .await
        .expect("watch");
    assert_eq!(watch.value().expect("view").entries.len(), 1);
    observation.detach.unsubscribe();
    watch.stop().await;
}

struct DetachingObserver {
    detach: Mutex<Option<Subscription>>,
    in_advance: std::sync::atomic::AtomicBool,
    ended: std::sync::atomic::AtomicBool,
    ended_after_advance: std::sync::atomic::AtomicBool,
}

impl Observer<ConversationView> for DetachingObserver {
    fn advance(
        &self,
        _call: WatchCall<ConversationView>,
        _context: &rpi_durable::chord::context::Context,
    ) {
        self.in_advance
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(detach) = self.detach.lock().expect("detach").take() {
            detach.unsubscribe();
        }
        self.in_advance
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn end(&self, _end: WatchEnd) {
        self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
        if !self.in_advance.load(std::sync::atomic::Ordering::SeqCst) {
            self.ended_after_advance
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observer_can_detach_inside_advance_without_deadlocking_the_writer() {
    let session = create_session(Arc::new(MemoryStorage::new()), None);
    let context = BACKGROUND_CONTEXT.clone();
    let conversation = session
        .commit(
            commit_fn(|tx| {
                Box::pin(async move {
                    Ok(tx
                        .create_conversation(ConversationOwnership::Ownerless)
                        .await?
                        .id)
                })
            }),
            &context,
        )
        .await
        .expect("conversation");
    let views = ConversationViews::new(Arc::clone(&session));
    let observer = Arc::new(DetachingObserver {
        detach: Mutex::new(None),
        in_advance: std::sync::atomic::AtomicBool::new(false),
        ended: std::sync::atomic::AtomicBool::new(false),
        ended_after_advance: std::sync::atomic::AtomicBool::new(false),
    });
    let observation = views
        .observe(
            conversation,
            Arc::clone(&observer) as Arc<dyn Observer<ConversationView>>,
            ConversationWindow::default(),
            &context,
        )
        .await
        .expect("observe");
    *observer.detach.lock().expect("detach") = Some(observation.detach);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.commit(
            commit_fn(move |tx| {
                Box::pin(async move {
                    tx.append_entry(conversation, EntryDraft::new("detach"))
                        .await?;
                    Ok(())
                })
            }),
            &context,
        ),
    )
    .await
    .expect("detach must not deadlock the writer")
    .expect("commit");
    assert!(
        observer.ended.load(std::sync::atomic::Ordering::SeqCst),
        "the detached observer still ends"
    );
    assert!(
        observer
            .ended_after_advance
            .load(std::sync::atomic::Ordering::SeqCst),
        "end must not run inside advance"
    );
}

/// A mount that hydrates while close begins registers nothing (upstream
/// `ConversationViews.attach` checks `#closed`), so the observer is not left
/// attached to a closed session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_racing_close_registers_nothing_and_rejects() {
    let session = create_session(Arc::new(MemoryStorage::new()), None);
    let context = BACKGROUND_CONTEXT.clone();
    let conversation = session
        .commit(
            commit_fn(|tx| {
                Box::pin(async move {
                    Ok(tx
                        .create_conversation(ConversationOwnership::Ownerless)
                        .await?
                        .id)
                })
            }),
            &context,
        )
        .await
        .expect("conversation");
    let views = ConversationViews::new(Arc::clone(&session));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let holding = tokio::spawn({
        let session = Arc::clone(&session);
        let context = context.clone();
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
                    &context,
                )
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the line is held");
    let observing = tokio::spawn({
        let views = Arc::clone(&views);
        let context = context.clone();
        async move {
            views
                .observe(
                    conversation,
                    Arc::new(RecordingObserver {
                        values: Arc::new(Mutex::new(Vec::new())),
                    }),
                    ConversationWindow::default(),
                    &context,
                )
                .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let closing = tokio::spawn({
        let session = Arc::clone(&session);
        let context = context.clone();
        async move { session.close(&context).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    release.notify_one();
    holding.await.unwrap().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), observing)
        .await
        .expect("the attach must not hang")
        .unwrap();
    assert!(matches!(result, Err(SessionError::Closed)));
    closing.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_custom_max_bounds_the_initial_from_window() {
    let session = create_session(Arc::new(MemoryStorage::new()), None);
    let context = BACKGROUND_CONTEXT.clone();
    let conversation = session
        .commit(
            commit_fn(|tx| {
                Box::pin(async move {
                    Ok(tx
                        .create_conversation(ConversationOwnership::Ownerless)
                        .await?
                        .id)
                })
            }),
            &context,
        )
        .await
        .expect("conversation");
    for index in 0..5 {
        session
            .commit(
                commit_fn(move |tx| {
                    Box::pin(async move {
                        tx.append_entry(conversation, EntryDraft::new(format!("entry-{index}")))
                            .await?;
                        Ok(())
                    })
                }),
                &context,
            )
            .await
            .expect("append");
    }
    let views = ConversationViews::new(Arc::clone(&session));
    let recording = Arc::new(Mutex::new(Vec::new()));
    let observation = views
        .observe(
            conversation,
            Arc::new(RecordingObserver {
                values: Arc::clone(&recording),
            }),
            ConversationWindow {
                // Entries 3..7 exist; from entry 4 leaves four, above max=3.
                from: Some(id_from_number(4)),
                min: Some(2),
                max: Some(3),
            },
            &context,
        )
        .await
        .expect("observe");
    // Upstream falls back to the newest min=2 and reports the entry before
    // them; using min*2 would have returned all four and exceeded max.
    assert_eq!(observation.value.entries.len(), 2);
    assert_eq!(observation.value.before, Some(id_from_number(5)));
    observation.detach.unsubscribe();
}

fn entry(id: u64) -> EntryRecord {
    EntryRecord::new(
        id_from_number::<rpi_durable::ids::Entry>(id),
        rpi_durable::ROOT_CONVERSATION_ID,
        "test",
    )
}

fn loader(entries: Vec<EntryRecord>) -> EntryLoader {
    Arc::new(move |query: EntryQuery, limit: usize, _context| {
        let mut items = entries
            .iter()
            .filter(|entry| {
                query.min_entry_id.is_none_or(|min| entry.id >= min)
                    && query.max_entry_id.is_none_or(|max| entry.id <= max)
            })
            .cloned()
            .collect::<Vec<_>>();
        if query.order == Some(ScanOrder::Descending) {
            items.reverse();
        }
        let items = items.into_iter().take(limit).collect::<Vec<_>>();
        let next = (items.len() == limit)
            .then(|| {
                items.last().map(|entry| {
                    Cursor::after_id(entry.id.get(), query.order.unwrap_or(ScanOrder::Ascending))
                })
            })
            .flatten();
        Box::pin(async move { Ok(Page { items, next }) })
    })
}

fn view(entries: Vec<EntryRecord>, before: Option<EntryId>) -> ConversationView {
    ConversationView {
        conversation: rpi_durable::types::ConversationRecord {
            id: rpi_durable::ROOT_CONVERSATION_ID,
            parent: None,
            owner: None,
        },
        entries,
        before,
        docs: BTreeMap::new(),
    }
}

#[tokio::test]
async fn transcript_loads_earlier_fills_gaps_and_resumes() {
    let entries = (1..=10).map(entry).collect::<Vec<_>>();
    let mut transcript = ConversationTranscript::new(loader(entries), Some(3)).expect("transcript");
    transcript.merge(view(
        vec![entry(3), entry(4), entry(5)],
        Some(id_from_number(2)),
    ));
    let transcript_view = transcript.merge(view(vec![entry(7), entry(8)], Some(id_from_number(6))));
    assert!(matches!(
        transcript_view.items.first(),
        Some(TranscriptItem::Earlier)
    ));
    assert!(transcript_view.items.iter().any(|item| matches!(item, TranscriptItem::Gap { after, up_to } if *after == id_from_number(5) && *up_to == id_from_number(7))));
    let filled = transcript
        .fill_gap(id_from_number(5), id_from_number(7), &BACKGROUND_CONTEXT)
        .await
        .expect("fill gap");
    assert!(
        !filled
            .items
            .iter()
            .any(|item| matches!(item, TranscriptItem::Gap { .. }))
    );
    let earlier = transcript
        .load_earlier(&BACKGROUND_CONTEXT)
        .await
        .expect("earlier");
    assert!(!matches!(
        earlier.items.first(),
        Some(TranscriptItem::Earlier)
    ));
    assert!(earlier.items.iter().any(
        |item| matches!(item, TranscriptItem::Entry { entry } if entry.id == id_from_number(1))
    ));
    assert_eq!(
        transcript.resume().and_then(|window| window.from),
        Some(id_from_number(9))
    );
}
