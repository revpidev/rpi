//! Harness submission admission/abort/wait integration tests (port of
//! `packages/durable/test/harness-submissions.test.ts` @ `4a080252f`).
//!
//! Scoped to what does not need the task scheduler: the recording scheduler
//! performs the `live.run` bookkeeping so the boundary and wait paths are
//! exercised, but no generation runs.

mod harness_support;

use std::sync::Arc;
use std::time::Duration;

use harness_support::{clear_run, context, input, input_with_entry, open_recording, write};
use rpi_durable::chord::context::with_abort_signal;
use rpi_durable::harness::{InputEntry, RootOptions, SubmissionAbort, WhenBusy};
use rpi_durable::session::EntryDraft;
use tokio_util::sync::CancellationToken;

fn note(kind: &str) -> EntryDraft {
    EntryDraft::new(kind)
}

async fn root_conversation(
    harness: &Arc<rpi_durable::harness::HarnessImpl>,
) -> Arc<rpi_durable::harness::ConversationImpl> {
    harness
        .root(&context(), RootOptions::default())
        .await
        .expect("root")
}

#[tokio::test]
async fn idle_write_appends_and_settles_done() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    let submission = conversation
        .submit(write(note("test.note"), None), &context)
        .await
        .expect("submit");
    let record = submission.status(&context).await.expect("status");
    assert_eq!(record.status(), "done");
}

#[tokio::test]
async fn idle_input_places_a_user_entry_and_starts_a_run() {
    let (harness, scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    let submission = conversation
        .submit(input("hello", None, None), &context)
        .await
        .expect("submit");
    assert_eq!(
        submission.status(&context).await.expect("status").status(),
        "placed"
    );
    let started = scheduler.started_runs();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, conversation.id());
    assert_eq!(started[0].1, vec![submission.id()]);
}

#[tokio::test]
async fn request_id_dedups_and_rejects_a_type_mismatch() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    let first = conversation
        .submit(input("one", None, Some("r1")), &context)
        .await
        .expect("first");
    let second = conversation
        .submit(input("two", None, Some("r1")), &context)
        .await
        .expect("second");
    assert_eq!(first.id(), second.id());
    let mismatch = conversation
        .submit(write(note("test.note"), Some("r1")), &context)
        .await;
    assert!(mismatch.is_err());
}

#[tokio::test]
async fn busy_reject_writes_nothing_and_follow_up_queues() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    conversation
        .submit(input("start", None, None), &context)
        .await
        .expect("start");
    let rejected = conversation
        .submit(input("nope", Some(WhenBusy::Reject), None), &context)
        .await;
    assert!(rejected.is_err());
    let queued = conversation
        .submit(input("later", Some(WhenBusy::FollowUp), None), &context)
        .await
        .expect("queue");
    assert_eq!(
        queued.status(&context).await.expect("status").status(),
        "queued"
    );
}

#[tokio::test]
async fn abort_reports_queued_placed_and_settled() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    let placed = conversation
        .submit(input("start", None, None), &context)
        .await
        .expect("start");
    assert_eq!(
        placed.abort(&context).await.expect("abort"),
        SubmissionAbort::AlreadyPlaced
    );
    let queued = conversation
        .submit(input("later", Some(WhenBusy::FollowUp), None), &context)
        .await
        .expect("queue");
    assert_eq!(
        queued.abort(&context).await.expect("abort"),
        SubmissionAbort::Aborted
    );
    // The settle left it terminal, so a second abort reports settled.
    assert_eq!(
        queued.abort(&context).await.expect("abort"),
        SubmissionAbort::Settled
    );
}

#[tokio::test]
async fn wait_resolves_for_a_settled_submission() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    let submission = conversation
        .submit(write(note("test.note"), None), &context)
        .await
        .expect("submit");
    let settled = submission.wait(&context).await.expect("wait");
    assert_eq!(settled.status(), "done");
}

#[tokio::test]
async fn close_rejects_a_pending_wait() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    conversation
        .submit(input("start", None, None), &context)
        .await
        .expect("start");
    let queued = conversation
        .submit(input("later", Some(WhenBusy::FollowUp), None), &context)
        .await
        .expect("queue");
    let waiting = {
        let queued = Arc::clone(&queued);
        let context = context.clone();
        tokio::spawn(async move { queued.wait(&context).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    harness.close(&context).await.expect("close");
    assert!(waiting.await.expect("join").is_err());
}

#[tokio::test]
async fn queued_items_are_placed_in_order_on_the_next_submission() {
    let (harness, scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    conversation
        .submit(input("start", None, None), &context)
        .await
        .expect("start");
    let queued = conversation
        .submit(input("second", Some(WhenBusy::FollowUp), None), &context)
        .await
        .expect("queue");
    // Stand in for the run ending; the earlier follow-up is still queued.
    clear_run(&harness, conversation.id()).await;
    conversation
        .submit(input("third", None, None), &context)
        .await
        .expect("third");
    let started = scheduler.started_runs();
    assert_eq!(started.len(), 2);
    assert_eq!(started[1].1, vec![queued.id()]);
    let record = queued.status(&context).await.expect("status");
    assert_eq!(record.status(), "placed");
}

/// upstream: "cancels only a wait ... the submission keeps its queue
/// position" — the wait rejects on its context cancellation while the durable
/// admission and the run are untouched.
#[tokio::test]
async fn cancelling_a_wait_does_not_cancel_the_queued_work() {
    let (harness, scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    conversation
        .submit(input("start", None, None), &context)
        .await
        .expect("start");
    let queued = conversation
        .submit(input("later", Some(WhenBusy::FollowUp), None), &context)
        .await
        .expect("queue");

    let token = CancellationToken::new();
    let cancellable = with_abort_signal(token.clone(), &context);
    let waiting = {
        let queued = Arc::clone(&queued);
        tokio::spawn(async move { queued.wait(&cancellable).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    token.cancel();

    assert!(
        waiting.await.expect("join").is_err(),
        "the cancelled wait rejects"
    );
    // The queued work survives: still queued, same run, no new admission.
    assert_eq!(
        queued.status(&context).await.expect("status").status(),
        "queued"
    );
    assert_eq!(scheduler.started_runs().len(), 1);
    harness.close(&context).await.expect("close");
}

/// R-D13: a display-only write is appended and settled done at once, even busy.
#[tokio::test]
async fn display_only_write_appends_while_busy() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    // Start a run so the conversation is busy.
    let running = conversation
        .submit(input("start", None, None), &context)
        .await
        .expect("start");
    assert_eq!(
        running.status(&context).await.expect("status").status(),
        "placed"
    );

    let display = conversation
        .submit(write(note("test.note"), None), &context)
        .await
        .expect("display write");
    assert_eq!(
        display.status(&context).await.expect("status").status(),
        "done",
        "a display-only write lands at once"
    );
    let entries = conversation
        .entries(
            rpi_durable::types::EntryQuery {
                conversation_id: conversation.id(),
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            50,
            None,
            &context,
        )
        .await
        .expect("entries");
    assert!(
        entries.items.iter().any(|entry| entry.kind == "test.note"),
        "the entry is in the transcript"
    );

    // A write that changes model context still queues behind the run.
    let mut contextual = note("test.context");
    contextual.model = Some(Vec::new());
    let queued = conversation
        .submit(write(contextual, None), &context)
        .await
        .expect("context write");
    assert_eq!(
        queued.status(&context).await.expect("status").status(),
        "queued"
    );
}

/// R-D13: an input's `entry` kind and data replace `pi.user`.
#[tokio::test]
async fn input_entry_places_the_application_kind() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    conversation
        .submit(
            input_with_entry(
                "hello",
                InputEntry {
                    kind: "app.message".to_string(),
                    data: Some(serde_json::json!({ "author": "ada" })),
                },
                None,
            ),
            &context,
        )
        .await
        .expect("submit");
    let entries = conversation
        .entries(
            rpi_durable::types::EntryQuery {
                conversation_id: conversation.id(),
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            50,
            None,
            &context,
        )
        .await
        .expect("entries");
    let entry = entries
        .items
        .iter()
        .find(|entry| entry.kind == "app.message")
        .expect("application entry");
    assert_eq!(entry.data, Some(serde_json::json!({ "author": "ada" })));
}

/// R-D13: an input entry kind may not use the reserved `pi.` prefix.
#[tokio::test]
async fn input_entry_rejects_reserved_kinds() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    let result = conversation
        .submit(
            input_with_entry(
                "hello",
                InputEntry {
                    kind: "pi.user".to_string(),
                    data: None,
                },
                None,
            ),
            &context,
        )
        .await;
    assert!(result.is_err(), "reserved kinds must be rejected");
}

/// R-D13: a queued reset holds a display-only write back.
#[tokio::test]
async fn a_queued_reset_holds_display_only_writes() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = root_conversation(&harness).await;
    conversation
        .submit(input("start", None, None), &context)
        .await
        .expect("start run");
    let mut reset = EntryDraft::new("pi.reset");
    reset.head = Some(rpi_durable::session::EntryHead::This);
    let queued_reset = conversation
        .submit(write(reset, None), &context)
        .await
        .expect("queued reset");
    assert_eq!(
        queued_reset
            .status(&context)
            .await
            .expect("status")
            .status(),
        "queued"
    );
    let display = conversation
        .submit(write(note("test.note"), None), &context)
        .await
        .expect("display write");
    assert_eq!(
        display.status(&context).await.expect("status").status(),
        "queued",
        "a display-only write behind a reset waits for the reset"
    );
}
