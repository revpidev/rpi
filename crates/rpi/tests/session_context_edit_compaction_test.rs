//! V16-03 FR-A R5/R7: context-edit-aware accounting and compaction
//! preparation. Ports the compaction intents of upstream
//! `test/session-context-edit.test.ts` @ 005af57d8 (`estimateProjectedContextTokens`,
//! `prepareCompaction` over the canonical projection).

use std::path::PathBuf;

use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi_agent::compaction::{
    CompactionSettings, DEFAULT_COMPACTION_SETTINGS, estimate_projected_context_tokens,
    prepare_compaction,
};
use rpi_agent::messages::AgentMessage;
use rpi_agent::session::SessionEntry;
use rpi_ai::types::{
    AssistantContent, AssistantMessage, AssistantRole, StopReason, TextContent, Usage, UserContent,
    UserMessage, UserRole,
};

fn in_memory() -> SessionManager {
    SessionManager::in_memory(None, NewSessionOptions::default()).expect("in-memory session")
}

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Text(text.to_owned()),
        timestamp: 1,
    })
}

fn assistant_with_usage(text: &str, input: u64, output: u64, total: u64) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        api: "faux".into(),
        provider: "faux".to_owned(),
        model: "faux-1".to_owned(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: None,
        usage: Usage {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            cache_write1h: None,
            reasoning: None,
            total_tokens: total,
            cost: Default::default(),
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 1,
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
    })
}

fn assistant(text: &str) -> AgentMessage {
    assistant_with_usage(text, 10, 1, 11)
}

fn typed_branch(manager: &SessionManager) -> Vec<SessionEntry> {
    manager
        .get_branch(None)
        .into_iter()
        .filter_map(|entry| entry.known().cloned())
        .collect()
}

fn projected_estimate(manager: &SessionManager) -> (u64, u64, u64) {
    let branch = typed_branch(manager);
    let projection = rpi_agent::session::build_session_projection(&branch);
    let estimate = estimate_projected_context_tokens(&projection, &branch);
    (
        estimate.tokens,
        estimate.usage_tokens,
        estimate.trailing_tokens,
    )
}

fn small_settings() -> CompactionSettings {
    CompactionSettings {
        keep_recent_tokens: 1,
        ..DEFAULT_COMPACTION_SETTINGS
    }
}

/// upstream: does not trust pre-edit assistant usage for projected context
/// estimates.
#[test]
fn projected_estimate_ignores_usage_invalidated_by_a_later_edit() {
    let mut session = in_memory();
    let large_user_id = session
        .append_message(user_msg(&"discarded input ".repeat(2_000)))
        .expect("append user");
    let assistant_id = session
        .append_message(assistant_with_usage("small answer", 10_000, 1, 10_001))
        .expect("append assistant");
    session
        .append_context_edit(&large_user_id, None)
        .expect("omit large input");

    let (tokens, usage_tokens, _) = projected_estimate(&session);
    assert_eq!(usage_tokens, 0);
    assert!(tokens < 100, "pure estimate expected, got {tokens}");

    session
        .append_context_edit(&assistant_id, None)
        .expect("omit assistant");
    let (tokens, _, _) = projected_estimate(&session);
    assert_eq!(tokens, 0);
}

/// upstream: uses assistant usage captured after the latest context edit.
#[test]
fn projected_estimate_trusts_usage_after_the_latest_edit() {
    let mut session = in_memory();
    let user_id = session
        .append_message(user_msg("original"))
        .expect("append user");
    session
        .append_context_edit(
            &user_id,
            Some(rpi_agent::session::ContextEditReplacement {
                content: rpi_agent::session::ContextEditableContent::Text("edited".to_owned()),
            }),
        )
        .expect("edit");
    session
        .append_message(assistant_with_usage("answer", 4_000, 100, 4_100))
        .expect("append assistant");
    session
        .append_message(user_msg("next"))
        .expect("append user");

    let (tokens, usage_tokens, trailing_tokens) = projected_estimate(&session);
    assert_eq!(usage_tokens, 4_100);
    assert_eq!(trailing_tokens, 1);
    assert_eq!(tokens, 4_101);
}

/// upstream: does not reuse post-edit assistant usage after a later
/// compaction.
#[test]
fn projected_estimate_ignores_usage_invalidated_by_a_later_compaction() {
    let mut session = in_memory();
    let user_id = session
        .append_message(user_msg("small input"))
        .expect("append user");
    session
        .append_context_edit(
            &user_id,
            Some(rpi_agent::session::ContextEditReplacement {
                content: rpi_agent::session::ContextEditableContent::Text(
                    "edited input".to_owned(),
                ),
            }),
        )
        .expect("edit");
    session
        .append_message(assistant_with_usage("answer", 50_000, 1, 50_001))
        .expect("append assistant");
    session
        .append_compaction("small summary", Some(&user_id), 50_001, None, None, None)
        .expect("compaction");

    let (tokens, usage_tokens, _) = projected_estimate(&session);
    assert_eq!(usage_tokens, 0);
    assert!(tokens < 100, "pure estimate expected, got {tokens}");
}

/// upstream: includes effective system and tool context in edited estimates.
#[test]
fn projected_estimate_includes_system_context() {
    let mut session = in_memory();
    session
        .append_message(AgentMessage::System(rpi_ai::types::SystemMessage {
            role: Default::default(),
            content: rpi_ai::types::SystemContent::Text("system prompt ".repeat(3_000)),
            sections: None,
            tools_added: Some(vec![rpi_ai::types::Tool {
                name: "example".to_owned(),
                description: "tool declaration ".repeat(100),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                constrained_sampling: None,
            }]),
            tools_removed: None,
            timestamp: 1,
        }))
        .expect("append system");
    let user_id = session
        .append_message(user_msg("ask"))
        .expect("append user");
    session
        .append_message(assistant("done"))
        .expect("append assistant");
    session
        .append_context_edit(
            &user_id,
            Some(rpi_agent::session::ContextEditReplacement {
                content: rpi_agent::session::ContextEditableContent::Text("ask".to_owned()),
            }),
        )
        .expect("edit");

    let (tokens, _, _) = projected_estimate(&session);
    assert!(tokens > 10_000, "system/tool context counted, got {tokens}");
}

/// upstream: does not advance past a boundary replacement of the candidate
/// input.
#[test]
fn prepare_compaction_does_not_advance_past_boundary_replacement() {
    let mut session = in_memory();
    session.append_message(user_msg("old request")).expect("u0");
    session.append_message(assistant("old answer")).expect("a0");
    let replaced_user_id = session
        .append_message(user_msg("original input"))
        .expect("u1");
    let assistant_id = session
        .append_message(assistant("answered original input"))
        .expect("a1");
    session
        .append_context_edit(
            &replaced_user_id,
            Some(rpi_agent::session::ContextEditReplacement {
                content: rpi_agent::session::ContextEditableContent::Text(
                    "NEW-INSTRUCTION ".repeat(100),
                ),
            }),
        )
        .expect("replace");
    session
        .append_context_edit(&assistant_id, None)
        .expect("omit");
    session
        .append_custom_entry("bookkeeping", Some(serde_json::json!({"source": "test"})))
        .expect("metadata");

    let preparation = prepare_compaction(&typed_branch(&session), &small_settings())
        .expect("preparation expected");
    assert_eq!(preparation.first_kept_entry_id, replaced_user_id);
    let summed = serde_json::to_string(&preparation.messages_to_summarize).expect("json");
    let prefix = serde_json::to_string(&preparation.turn_prefix_messages).expect("json");
    assert!(!summed.contains("NEW-INSTRUCTION"), "summarize: {summed}");
    assert!(!prefix.contains("NEW-INSTRUCTION"), "prefix: {prefix}");
}

/// upstream: does not treat an omitted custom message as a recovery attempt.
#[test]
fn prepare_compaction_does_not_treat_omitted_custom_message_as_recovery() {
    let mut session = in_memory();
    session
        .append_message(user_msg(&"unanswered input ".repeat(100)))
        .expect("user");
    let custom_id = session
        .append_custom_message_entry(
            "temporary",
            UserContent::Text("temporary context".to_owned()),
            false,
            None,
        )
        .expect("custom message");
    session
        .append_context_edit(&custom_id, None)
        .expect("omit custom");

    let preparation = prepare_compaction(&typed_branch(&session), &small_settings());
    assert!(
        preparation.is_none(),
        "nothing to summarize: {preparation:?}"
    );
}

/// upstream: advances past input for an omitted assistant recovery suffix
/// with metadata.
#[test]
fn prepare_compaction_advances_past_omitted_assistant_recovery_suffix() {
    let mut session = in_memory();
    let user_id = session
        .append_message(user_msg(&"recovery input ".repeat(100)))
        .expect("user");
    let attempt_id = session
        .append_message(assistant("failed attempt"))
        .expect("attempt");
    session
        .append_context_edit(&attempt_id, None)
        .expect("omit attempt");
    session
        .append_custom_entry("bookkeeping", Some(serde_json::json!({"source": "test"})))
        .expect("metadata");

    let preparation = prepare_compaction(&typed_branch(&session), &small_settings())
        .expect("preparation expected");
    assert_eq!(preparation.first_kept_entry_id, attempt_id);
    assert_ne!(user_id, attempt_id);
    assert_eq!(preparation.turn_prefix_messages.len(), 1);
    let prefix = serde_json::to_string(&preparation.turn_prefix_messages).expect("json");
    assert!(prefix.contains("recovery input"), "prefix: {prefix}");
    let summed = serde_json::to_string(&preparation.messages_to_summarize).expect("json");
    assert!(!summed.contains("recovery input"), "summarize: {summed}");
}

/// upstream: prepares compaction from edited model content.
#[test]
fn prepare_compaction_uses_edited_model_content() {
    let mut session = in_memory();
    let omitted_id = session
        .append_message(user_msg(&"OMIT-ME ".repeat(100)))
        .expect("omitted");
    session
        .append_message(assistant(&"old answer ".repeat(100)))
        .expect("assistant");
    session
        .append_context_edit(&omitted_id, None)
        .expect("omit");
    session.append_message(user_msg("keep")).expect("keep");
    session.append_message(assistant("suffix")).expect("suffix");

    let preparation = prepare_compaction(&typed_branch(&session), &small_settings())
        .expect("preparation expected");
    let summed = serde_json::to_string(&preparation.messages_to_summarize).expect("json");
    let prefix = serde_json::to_string(&preparation.turn_prefix_messages).expect("json");
    assert!(!summed.contains("OMIT-ME"), "summarize: {summed}");
    assert!(!prefix.contains("OMIT-ME"), "prefix: {prefix}");
}

/// TempDir helper kept local so the test file is self-contained.
#[allow(dead_code)]
struct TempDir(PathBuf);
