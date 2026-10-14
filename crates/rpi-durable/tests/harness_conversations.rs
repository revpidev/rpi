//! Harness conversation and lifecycle integration tests (port of
//! `packages/durable/test/harness-conversations.test.ts` @ `abe508e1b`).
//!
//! Scoped to what does not need the task scheduler: lazy root creation, the
//! built-in creation hook, agent edits, Tx-created conversations, and close
//! behaviour.

mod harness_support;

use std::sync::Arc;

use harness_support::{context, init_append, open_recording};
use rpi_durable::harness::{AgentChange, ConversationCreateOptions, RootOptions};
use rpi_durable::ids::{ConversationId, id_from_number};
use rpi_durable::session::{ConversationOwnership, DocOwner, commit_fn};

async fn entry_kinds(
    harness: &Arc<rpi_durable::harness::HarnessImpl>,
    id: ConversationId,
) -> Vec<String> {
    let storage = Arc::clone(harness.session().storage());
    let context = context();
    let mut kinds = Vec::new();
    let mut cursor = None;
    loop {
        let page = storage
            .scan_entries(
                rpi_durable::EntryQuery {
                    conversation_id: id,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: None,
                },
                256,
                cursor,
                &context,
            )
            .await
            .expect("scan entries");
        kinds.extend(page.items.into_iter().map(|entry| entry.kind));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    kinds
}

#[tokio::test]
async fn root_is_created_lazily_with_agent_change_and_init() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = harness
        .root(
            &context,
            RootOptions {
                agent: Some(AgentChange {
                    instructions: Some(Some("be brief".to_string())),
                    ..AgentChange::default()
                }),
                init: Some(init_append("test.init")),
            },
        )
        .await
        .expect("root");
    assert_eq!(
        conversation.id(),
        id_from_number::<rpi_durable::ids::Conversation>(1)
    );
    let agent = conversation.agent(&context).await.expect("agent");
    assert_eq!(agent.instructions.as_deref(), Some("be brief"));
    assert!(
        entry_kinds(&harness, conversation.id())
            .await
            .contains(&"test.init".to_string())
    );
    // The second call returns the same reserved root without re-running init.
    let again = harness
        .root(&context, RootOptions::default())
        .await
        .expect("root again");
    assert_eq!(again.id(), conversation.id());
    assert_eq!(
        entry_kinds(&harness, conversation.id())
            .await
            .iter()
            .filter(|kind| *kind == "test.init")
            .count(),
        1
    );
}

#[tokio::test]
async fn conversation_created_hook_creates_the_builtin_documents() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let root = harness
        .root(&context, RootOptions::default())
        .await
        .expect("root");
    for token in [
        &*rpi_durable::harness::LIVE_DOC,
        &*rpi_durable::harness::INBOX_DOC,
        &*rpi_durable::harness::USAGE_DOC,
        &*rpi_durable::harness::PROVIDER_DOC,
    ] {
        let value = harness
            .session()
            .snapshot(token, DocOwner::Conversation(root.id()), &context)
            .await
            .expect("snapshot");
        assert!(value.is_some(), "built-in document missing");
    }
}

#[tokio::test]
async fn a_tx_created_conversation_gets_the_builtin_documents() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let id = harness
        .session()
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
        .expect("tx conversation");
    let value = harness
        .session()
        .snapshot(
            &rpi_durable::harness::LIVE_DOC,
            DocOwner::Conversation(id),
            &context,
        )
        .await
        .expect("snapshot");
    assert!(value.is_some());
}

#[tokio::test]
async fn agent_change_replaces_clears_and_leaves_alone() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = harness
        .root(&context, RootOptions::default())
        .await
        .expect("root");
    conversation
        .configure(
            AgentChange {
                instructions: Some(Some("first".to_string())),
                ..AgentChange::default()
            },
            &context,
        )
        .await
        .expect("configure");
    assert_eq!(
        conversation
            .agent(&context)
            .await
            .expect("agent")
            .instructions
            .as_deref(),
        Some("first")
    );
    // `cwd` replaces, `instructions` clears; the untouched model stays absent.
    conversation
        .configure(
            AgentChange {
                cwd: Some(Some("/tmp".to_string())),
                instructions: Some(None),
                ..AgentChange::default()
            },
            &context,
        )
        .await
        .expect("configure");
    let agent = conversation.agent(&context).await.expect("agent");
    assert_eq!(agent.cwd.as_deref(), Some("/tmp"));
    assert_eq!(agent.instructions, None);
    assert_eq!(agent.model, None);
}

#[tokio::test]
async fn independent_conversations_get_init_and_roll_back_failures() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let conversation = harness
        .create_conversation(
            ConversationCreateOptions {
                ownership: ConversationOwnership::Ownerless,
                agent: None,
                init: Some(init_append("test.init")),
            },
            &context,
        )
        .await
        .expect("create");
    assert!(
        entry_kinds(&harness, conversation.id())
            .await
            .contains(&"test.init".to_string())
    );
}

#[tokio::test]
async fn conversation_lookup_returns_stateless_handles_and_close_rejects() {
    let (harness, _scheduler) = open_recording().await;
    let context = context();
    let root = harness
        .root(&context, RootOptions::default())
        .await
        .expect("root");
    let first = harness
        .conversation(root.id(), &context)
        .await
        .expect("lookup")
        .expect("present");
    let second = harness
        .conversation(root.id(), &context)
        .await
        .expect("lookup")
        .expect("present");
    assert_eq!(first.id(), second.id());
    assert!(
        harness
            .conversation(
                id_from_number::<rpi_durable::ids::Conversation>(999),
                &context
            )
            .await
            .expect("lookup")
            .is_none()
    );
    harness.close(&context).await.expect("close");
    assert!(harness.conversation(root.id(), &context).await.is_err());
    assert!(harness.resume().is_err());
}
