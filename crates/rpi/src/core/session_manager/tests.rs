//! Ported test intents from `test/session-manager/*.test.ts` @ pi 0.82.1
//! (2efa728), plus T07 self-check anchors (migration rewrite, deferred
//! persistence, unknown-entry lossless round-trip, retainedTail form,
//! fork positions, append-write failure propagation).
//!
//! Upstream test "opens session files larger than Node's max string length"
//! (file-operations.test.ts:120-147) is not ported: it needs a >512MB sparse
//! file and exercises a V8 string limit that has no Rust equivalent — the
//! streaming reader is already covered by the scan-limit and malformed-line
//! tests.

use rpi_agent::messages::AgentMessage;
use rpi_agent::session::{CompactionEntry, SessionEntry};
use rpi_ai::types::{
    AssistantContent, AssistantMessage, AssistantRole, StopReason, TextContent, Usage, UserContent,
    UserContentBlock, UserMessage, UserRole,
};
use serde_json::{json, Value};

use super::*;
use crate::tools::test_helpers::TempDir;

// ---------------------------------------------------------------------------
// Entry factories (mirrors test/utilities.ts + build-context.test.ts helpers)
// ---------------------------------------------------------------------------

const TS: &str = "2025-01-01T00:00:00.000Z";

fn known(entry: SessionEntry) -> StoredEntry {
    let raw = serde_json::to_value(&entry).expect("serialize SessionEntry");
    StoredEntry::Known {
        typed: Box::new(entry),
        raw,
    }
}

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Text(text.to_owned()),
        timestamp: 1,
    })
}

fn assistant_msg(text: &str) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        api: "anthropic-messages".into(),
        provider: "anthropic".to_owned(),
        model: "claude-test".to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage {
            input: 1,
            output: 1,
            cache_read: 0,
            cache_write: 0,
            cache_write1h: None,
            reasoning: None,
            total_tokens: 2,
            cost: Default::default(),
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 1,
    })
}

fn msg(id: &str, parent_id: Option<&str>, message: AgentMessage) -> StoredEntry {
    known(SessionEntry::Message(MessageEntry {
        id: id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp: TS.to_owned(),
        message,
    }))
}

fn compaction(id: &str, parent_id: Option<&str>, summary: &str, first_kept: &str) -> StoredEntry {
    known(SessionEntry::Compaction(CompactionEntry {
        id: id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp: TS.to_owned(),
        summary: summary.to_owned(),
        first_kept_entry_id: Some(first_kept.to_owned()),
        tokens_before: 1000,
        retained_tail: None,
        details: None,
        usage: None,
        from_hook: None,
    }))
}

fn branch_summary(id: &str, parent_id: Option<&str>, summary: &str, from_id: &str) -> StoredEntry {
    known(SessionEntry::BranchSummary(BranchSummaryEntry {
        id: id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp: TS.to_owned(),
        from_id: from_id.to_owned(),
        summary: summary.to_owned(),
        details: None,
        usage: None,
        from_hook: None,
    }))
}

fn custom(
    id: &str,
    parent_id: Option<&str>,
    custom_type: &str,
    data: Option<Value>,
) -> StoredEntry {
    known(SessionEntry::Custom(CustomEntry {
        id: id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp: TS.to_owned(),
        custom_type: custom_type.to_owned(),
        data,
    }))
}

fn thinking_level(id: &str, parent_id: Option<&str>, level: &str) -> StoredEntry {
    known(SessionEntry::ThinkingLevelChange(
        ThinkingLevelChangeEntry {
            id: id.to_owned(),
            parent_id: parent_id.map(str::to_owned),
            timestamp: TS.to_owned(),
            thinking_level: level.to_owned(),
        },
    ))
}

fn model_change(id: &str, parent_id: Option<&str>, provider: &str, model_id: &str) -> StoredEntry {
    known(SessionEntry::ModelChange(ModelChangeEntry {
        id: id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp: TS.to_owned(),
        provider: provider.to_owned(),
        model_id: model_id.to_owned(),
    }))
}

fn entry_ids(entries: &[StoredEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.id()).collect()
}

fn msg_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(u) => match &u.content {
            UserContent::Text(s) => s.clone(),
            UserContent::Blocks(b) => match &b[0] {
                UserContentBlock::Text(t) => t.text.clone(),
                _ => panic!("expected text block"),
            },
        },
        AgentMessage::Assistant(a) => match &a.content[0] {
            AssistantContent::Text(t) => t.text.clone(),
            _ => panic!("expected text block"),
        },
        _ => panic!("expected user/assistant message"),
    }
}

fn usage_sample() -> Usage {
    Usage {
        input: 10,
        output: 20,
        cache_read: 30,
        cache_write: 40,
        cache_write1h: None,
        reasoning: None,
        total_tokens: 100,
        cost: rpi_ai::types::UsageCost {
            input: 0.1,
            output: 0.2,
            cache_read: 0.3,
            cache_write: 0.4,
            total: 1.0,
        },
    }
}

fn in_memory() -> SessionManager {
    SessionManager::in_memory(None, NewSessionOptions::default())
        .expect("in-memory session must construct")
}

fn header_line(id: &str, cwd: &str) -> String {
    format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"{TS}\",\"cwd\":\"{cwd}\"}}")
}

fn user_line(id: &str, parent: &str, text: &str) -> String {
    let parent = if parent.is_empty() {
        "null".to_owned()
    } else {
        format!("\"{parent}\"")
    };
    format!(
        "{{\"type\":\"message\",\"id\":\"{id}\",\"parentId\":{parent},\"timestamp\":\"{TS}\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\",\"timestamp\":1}}}}"
    )
}

// ===========================================================================
// build-context.test.ts
// ===========================================================================

#[test]
fn empty_entries_returns_empty_context() {
    let ctx = build_session_context(&[], None);
    assert!(ctx.messages.is_empty());
    assert_eq!(ctx.thinking_level, "off");
    assert_eq!(ctx.model, None);
}

#[test]
fn single_user_message() {
    let entries = vec![msg("1", None, user_msg("hello"))];
    let ctx = build_session_context(&entries, None);
    assert_eq!(ctx.messages.len(), 1);
    assert!(matches!(ctx.messages[0], AgentMessage::User(_)));
}

#[test]
fn simple_conversation() {
    let entries = vec![
        msg("1", None, user_msg("hello")),
        msg("2", Some("1"), assistant_msg("hi there")),
        msg("3", Some("2"), user_msg("how are you")),
        msg("4", Some("3"), assistant_msg("great")),
    ];
    let ctx = build_session_context(&entries, None);
    assert_eq!(ctx.messages.len(), 4);
    let roles: Vec<&str> = ctx
        .messages
        .iter()
        .map(|m| match m {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            _ => "other",
        })
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
}

#[test]
fn tracks_thinking_level_changes() {
    let entries = vec![
        msg("1", None, user_msg("hello")),
        thinking_level("2", Some("1"), "high"),
        msg("3", Some("2"), assistant_msg("thinking hard")),
    ];
    let ctx = build_session_context(&entries, None);
    assert_eq!(ctx.thinking_level, "high");
    assert_eq!(ctx.messages.len(), 2);
}

#[test]
fn tracks_model_from_assistant_message() {
    let entries = vec![
        msg("1", None, user_msg("hello")),
        msg("2", Some("1"), assistant_msg("hi")),
    ];
    let ctx = build_session_context(&entries, None);
    assert_eq!(
        ctx.model,
        Some(SessionModel {
            provider: "anthropic".to_owned(),
            model_id: "claude-test".to_owned()
        })
    );
}

#[test]
fn tracks_model_from_model_change_entry() {
    let entries = vec![
        msg("1", None, user_msg("hello")),
        model_change("2", Some("1"), "openai", "gpt-4"),
        msg("3", Some("2"), assistant_msg("hi")),
    ];
    let ctx = build_session_context(&entries, None);
    // Assistant message overwrites model change.
    assert_eq!(
        ctx.model,
        Some(SessionModel {
            provider: "anthropic".to_owned(),
            model_id: "claude-test".to_owned()
        })
    );
}

#[test]
fn includes_summary_before_kept_messages() {
    let entries = vec![
        msg("1", None, user_msg("first")),
        msg("2", Some("1"), assistant_msg("response1")),
        msg("3", Some("2"), user_msg("second")),
        msg("4", Some("3"), assistant_msg("response2")),
        compaction("5", Some("4"), "Summary of first two turns", "3"),
        msg("6", Some("5"), user_msg("third")),
        msg("7", Some("6"), assistant_msg("response3")),
    ];
    let ctx = build_session_context(&entries, None);
    assert_eq!(ctx.messages.len(), 5);
    match &ctx.messages[0] {
        AgentMessage::CompactionSummary(c) => {
            assert!(c.summary.contains("Summary of first two turns"))
        }
        other => panic!("expected compactionSummary, got {other:?}"),
    }
    assert_eq!(msg_text(&ctx.messages[1]), "second");
    assert_eq!(msg_text(&ctx.messages[2]), "response2");
    assert_eq!(msg_text(&ctx.messages[3]), "third");
    assert_eq!(msg_text(&ctx.messages[4]), "response3");
}

#[test]
fn handles_compaction_keeping_from_first_message() {
    let entries = vec![
        msg("1", None, user_msg("first")),
        msg("2", Some("1"), assistant_msg("response")),
        compaction("3", Some("2"), "Empty summary", "1"),
        msg("4", Some("3"), user_msg("second")),
    ];
    let ctx = build_session_context(&entries, None);
    assert_eq!(ctx.messages.len(), 4);
    match &ctx.messages[0] {
        AgentMessage::CompactionSummary(c) => assert!(c.summary.contains("Empty summary")),
        other => panic!("expected compactionSummary, got {other:?}"),
    }
}

#[test]
fn multiple_compactions_uses_latest() {
    let entries = vec![
        msg("1", None, user_msg("a")),
        msg("2", Some("1"), assistant_msg("b")),
        compaction("3", Some("2"), "First summary", "1"),
        msg("4", Some("3"), user_msg("c")),
        msg("5", Some("4"), assistant_msg("d")),
        compaction("6", Some("5"), "Second summary", "4"),
        msg("7", Some("6"), user_msg("e")),
    ];
    let ctx = build_session_context(&entries, None);
    assert_eq!(ctx.messages.len(), 4);
    match &ctx.messages[0] {
        AgentMessage::CompactionSummary(c) => assert!(c.summary.contains("Second summary")),
        other => panic!("expected compactionSummary, got {other:?}"),
    }
}

#[test]
fn build_context_entries_returns_compaction_aware_entries_including_custom_entries() {
    let entries = vec![
        msg("1", None, user_msg("first")),
        custom("2", Some("1"), "old-state", Some(json!({"hidden": true}))),
        msg("3", Some("2"), assistant_msg("response1")),
        custom("4", Some("3"), "kept-card", Some(json!({"title": "Kept"}))),
        msg("5", Some("4"), user_msg("second")),
        compaction("6", Some("5"), "Summary", "4"),
        custom(
            "7",
            Some("6"),
            "after-card",
            Some(json!({"title": "After"})),
        ),
        msg("8", Some("7"), assistant_msg("response2")),
    ];

    assert_eq!(
        entry_ids(&build_context_entries(&entries, None)),
        ["6", "4", "5", "7", "8"]
    );
    let ctx = build_session_context(&entries, None);
    let roles: Vec<&str> = ctx
        .messages
        .iter()
        .map(|m| match m {
            AgentMessage::CompactionSummary(_) => "compactionSummary",
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            _ => "other",
        })
        .collect();
    assert_eq!(roles, ["compactionSummary", "user", "assistant"]);
}

#[test]
fn keeps_settings_from_the_full_path_after_compaction() {
    let entries = vec![
        msg("1", None, user_msg("first")),
        thinking_level("2", Some("1"), "high"),
        msg("3", Some("2"), assistant_msg("response1")),
        msg("4", Some("3"), user_msg("second")),
        compaction("5", Some("4"), "Summary", "4"),
    ];
    let ctx = build_session_context(&entries, None);
    assert_eq!(ctx.thinking_level, "high");
    assert_eq!(ctx.messages.len(), 2);
    assert!(matches!(
        ctx.messages[0],
        AgentMessage::CompactionSummary(_)
    ));
    assert!(matches!(ctx.messages[1], AgentMessage::User(_)));
}

#[test]
fn follows_path_to_specified_leaf() {
    let entries = vec![
        msg("1", None, user_msg("start")),
        msg("2", Some("1"), assistant_msg("response")),
        msg("3", Some("2"), user_msg("branch A")),
        msg("4", Some("2"), user_msg("branch B")),
    ];
    let ctx_a = build_session_context(&entries, Some("3"));
    assert_eq!(ctx_a.messages.len(), 3);
    assert_eq!(msg_text(&ctx_a.messages[2]), "branch A");

    let ctx_b = build_session_context(&entries, Some("4"));
    assert_eq!(ctx_b.messages.len(), 3);
    assert_eq!(msg_text(&ctx_b.messages[2]), "branch B");
}

#[test]
fn includes_branch_summary_in_path() {
    let entries = vec![
        msg("1", None, user_msg("start")),
        msg("2", Some("1"), assistant_msg("response")),
        msg("3", Some("2"), user_msg("abandoned path")),
        branch_summary("4", Some("2"), "Summary of abandoned work", "3"),
        msg("5", Some("4"), user_msg("new direction")),
    ];
    let ctx = build_session_context(&entries, Some("5"));
    assert_eq!(ctx.messages.len(), 4);
    match &ctx.messages[2] {
        AgentMessage::BranchSummary(b) => {
            assert!(b.summary.contains("Summary of abandoned work"))
        }
        other => panic!("expected branchSummary, got {other:?}"),
    }
    assert_eq!(msg_text(&ctx.messages[3]), "new direction");
}

#[test]
fn complex_tree_with_multiple_branches_and_compaction() {
    let entries = vec![
        msg("1", None, user_msg("start")),
        msg("2", Some("1"), assistant_msg("r1")),
        msg("3", Some("2"), user_msg("q2")),
        msg("4", Some("3"), assistant_msg("r2")),
        compaction("5", Some("4"), "Compacted history", "3"),
        msg("6", Some("5"), user_msg("q3")),
        msg("7", Some("6"), assistant_msg("r3")),
        // Abandoned branch from 3.
        msg("8", Some("3"), user_msg("wrong path")),
        msg("9", Some("8"), assistant_msg("wrong response")),
        // Branch summary resuming from 3.
        branch_summary("10", Some("3"), "Tried wrong approach", "9"),
        msg("11", Some("10"), user_msg("better approach")),
    ];

    // Main path to 7: summary + kept(3,4) + after(6,7).
    let ctx_main = build_session_context(&entries, Some("7"));
    assert_eq!(ctx_main.messages.len(), 5);
    match &ctx_main.messages[0] {
        AgentMessage::CompactionSummary(c) => assert!(c.summary.contains("Compacted history")),
        other => panic!("expected compactionSummary, got {other:?}"),
    }
    assert_eq!(msg_text(&ctx_main.messages[1]), "q2");
    assert_eq!(msg_text(&ctx_main.messages[2]), "r2");
    assert_eq!(msg_text(&ctx_main.messages[3]), "q3");
    assert_eq!(msg_text(&ctx_main.messages[4]), "r3");

    // Branch path to 11: 1,2,3 + branch_summary + 11.
    let ctx_branch = build_session_context(&entries, Some("11"));
    assert_eq!(ctx_branch.messages.len(), 5);
    assert_eq!(msg_text(&ctx_branch.messages[0]), "start");
    assert_eq!(msg_text(&ctx_branch.messages[1]), "r1");
    assert_eq!(msg_text(&ctx_branch.messages[2]), "q2");
    match &ctx_branch.messages[3] {
        AgentMessage::BranchSummary(b) => assert!(b.summary.contains("Tried wrong approach")),
        other => panic!("expected branchSummary, got {other:?}"),
    }
    assert_eq!(msg_text(&ctx_branch.messages[4]), "better approach");
}

#[test]
fn uses_last_entry_when_leaf_id_not_found() {
    let entries = vec![
        msg("1", None, user_msg("hello")),
        msg("2", Some("1"), assistant_msg("hi")),
    ];
    let ctx = build_session_context(&entries, Some("nonexistent"));
    assert_eq!(ctx.messages.len(), 2);
}

#[test]
fn handles_orphaned_entries_gracefully() {
    let entries = vec![
        msg("1", None, user_msg("hello")),
        msg("2", Some("missing"), assistant_msg("orphan")),
    ];
    let ctx = build_session_context(&entries, Some("2"));
    assert_eq!(ctx.messages.len(), 1);
}

/// retainedTail form (session-format.md §Context Building, harness
/// session.ts:72-77/123-127; T07 self-check: both forms).
#[test]
fn retained_tail_compaction_form_is_self_contained_checkpoint() {
    let tail = vec![user_msg("latest request"), assistant_msg("latest reply")];
    let entries = vec![
        msg("1", None, user_msg("old first")),
        msg("2", Some("1"), assistant_msg("old response")),
        known(SessionEntry::Compaction(CompactionEntry {
            id: "3".to_owned(),
            parent_id: Some("2".to_owned()),
            timestamp: TS.to_owned(),
            summary: "Retained tail summary".to_owned(),
            first_kept_entry_id: None,
            tokens_before: 5000,
            retained_tail: Some(tail),
            details: None,
            usage: None,
            from_hook: None,
        })),
        msg("4", Some("3"), user_msg("after compaction")),
    ];

    // Entry selection: compaction + entries after it; nothing before is
    // walked (no firstKeptEntryId).
    assert_eq!(
        entry_ids(&build_context_entries(&entries, None)),
        ["3", "4"]
    );

    // Messages: compactionSummary + retainedTail + after.
    let ctx = build_session_context(&entries, None);
    assert_eq!(ctx.messages.len(), 4);
    match &ctx.messages[0] {
        AgentMessage::CompactionSummary(c) => {
            assert!(c.summary.contains("Retained tail summary"));
            assert_eq!(c.tokens_before, 5000);
        }
        other => panic!("expected compactionSummary, got {other:?}"),
    }
    assert_eq!(msg_text(&ctx.messages[1]), "latest request");
    assert_eq!(msg_text(&ctx.messages[2]), "latest reply");
    assert_eq!(msg_text(&ctx.messages[3]), "after compaction");
}

// ===========================================================================
// migration.test.ts + T07 migration anchors
// ===========================================================================

#[test]
fn should_add_id_parent_id_to_v1_entries() {
    let mut entries = vec![
        json!({"type":"session","id":"sess-1","timestamp":TS,"cwd":"/tmp"}),
        json!({"type":"message","timestamp":TS,"message":{"role":"user","content":"hi","timestamp":1}}),
        json!({"type":"message","timestamp":TS,"message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"api":"test","provider":"test","model":"test","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}),
    ];
    migrate_session_entries(&mut entries);

    // Header has version set (v3 current after hookMessage→custom migration).
    assert_eq!(entries[0]["version"], json!(3));

    let msg1 = &entries[1];
    let msg2 = &entries[2];
    assert_eq!(msg1["id"].as_str().map(str::len), Some(8));
    assert_eq!(msg1["parentId"], Value::Null);
    assert_eq!(msg2["id"].as_str().map(str::len), Some(8));
    assert_eq!(msg2["parentId"], msg1["id"]);
}

#[test]
fn should_be_idempotent_skip_already_migrated() {
    let mut entries = vec![
        json!({"type":"session","id":"sess-1","version":2,"timestamp":TS,"cwd":"/tmp"}),
        json!({"type":"message","id":"abc12345","parentId":null,"timestamp":TS,"message":{"role":"user","content":"hi","timestamp":1}}),
    ];
    migrate_session_entries(&mut entries);
    assert_eq!(entries[0]["version"], json!(3));
    assert_eq!(entries[1]["id"], json!("abc12345"));
    assert_eq!(entries[1]["parentId"], Value::Null);
}

/// v1→v2: `firstKeptEntryIndex` numeric index → `firstKeptEntryId`
/// (T07 self-check: includes the firstKeptEntryIndex conversion).
#[test]
fn converts_first_kept_entry_index_to_first_kept_entry_id() {
    let mut entries = vec![
        json!({"type":"session","id":"sess-1","timestamp":TS,"cwd":"/tmp"}),
        json!({"type":"message","timestamp":TS,"message":{"role":"user","content":"first","timestamp":1}}),
        json!({"type":"message","timestamp":TS,"message":{"role":"user","content":"second","timestamp":2}}),
        json!({"type":"compaction","timestamp":TS,"summary":"s","firstKeptEntryIndex":2,"tokensBefore":100}),
    ];
    migrate_session_entries(&mut entries);

    let compaction = &entries[3];
    // Index 2 → the second message entry; its id assigned earlier in the pass.
    assert_eq!(compaction["firstKeptEntryId"], entries[2]["id"]);
    assert!(compaction.get("firstKeptEntryIndex").is_none());
}

/// v2→v3: `hookMessage` role → `custom`.
#[test]
fn renames_hook_message_role_to_custom() {
    let mut entries = vec![
        json!({"type":"session","id":"sess-1","version":2,"timestamp":TS,"cwd":"/tmp"}),
        json!({"type":"message","id":"m1","parentId":null,"timestamp":TS,"message":{"role":"hookMessage","hookType":"x","content":"note","timestamp":1}}),
    ];
    migrate_session_entries(&mut entries);
    assert_eq!(entries[0]["version"], json!(3));
    assert_eq!(entries[1]["message"]["role"], json!("custom"));
}

/// After migration the whole file is rewritten (session-format.md §Session Version;
/// T07 self-check).
#[test]
fn migrated_v1_file_is_rewritten_on_open() {
    let tmp = TempDir::new();
    let file = tmp.path().join("v1.jsonl");
    let content = concat!(
        "{\"type\":\"session\",\"id\":\"sess-1\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"timestamp\":\"2025-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
    );
    std::fs::write(&file, content).expect("write v1 file");

    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open v1 session");
    assert_eq!(sm.get_session_id(), "sess-1");

    // File rewritten to v3 with id/parentId assigned.
    let rewritten = std::fs::read_to_string(&file).expect("read rewritten");
    let lines: Vec<&str> = rewritten.trim().split('\n').collect();
    assert_eq!(lines.len(), 2);
    let header: Value = serde_json::from_str(lines[0]).expect("header json");
    assert_eq!(header["version"], json!(3));
    let entry: Value = serde_json::from_str(lines[1]).expect("entry json");
    assert!(entry["id"].is_string());
    assert_eq!(entry["parentId"], Value::Null);

    // Leaf points at the migrated entry.
    assert_eq!(sm.get_leaf_id(), entry["id"].as_str());
}

// ===========================================================================
// file-operations.test.ts
// ===========================================================================

#[test]
fn load_entries_returns_empty_for_nonexistent_file() {
    let tmp = TempDir::new();
    assert!(load_entries_from_file(&tmp.path().join("nonexistent.jsonl")).is_empty());
}

#[test]
fn load_entries_returns_empty_for_empty_file() {
    let tmp = TempDir::new();
    let file = tmp.path().join("empty.jsonl");
    std::fs::write(&file, "").expect("write");
    assert!(load_entries_from_file(&file).is_empty());
}

#[test]
fn load_entries_returns_empty_for_file_without_valid_session_header() {
    let tmp = TempDir::new();
    let file = tmp.path().join("no-header.jsonl");
    std::fs::write(&file, "{\"type\":\"message\",\"id\":\"1\"}\n").expect("write");
    assert!(load_entries_from_file(&file).is_empty());
}

#[test]
fn load_entries_returns_empty_for_malformed_json() {
    let tmp = TempDir::new();
    let file = tmp.path().join("malformed.jsonl");
    std::fs::write(&file, "not json\n").expect("write");
    assert!(load_entries_from_file(&file).is_empty());
}

#[test]
fn load_entries_loads_valid_session_file() {
    let tmp = TempDir::new();
    let file = tmp.path().join("valid.jsonl");
    std::fs::write(
        &file,
        format!(
            "{}\n{}\n",
            header_line("abc", "/tmp"),
            user_line("1", "", "hi")
        ),
    )
    .expect("write");
    let entries = load_entries_from_file(&file);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["type"], json!("session"));
    assert_eq!(entries[1]["type"], json!("message"));
}

#[test]
fn load_entries_skips_malformed_lines_but_keeps_valid_ones() {
    let tmp = TempDir::new();
    let file = tmp.path().join("mixed.jsonl");
    std::fs::write(
        &file,
        format!(
            "{}\nnot valid json\n{}\n",
            header_line("abc", "/tmp"),
            user_line("1", "", "hi")
        ),
    )
    .expect("write");
    assert_eq!(load_entries_from_file(&file).len(), 2);
}

#[test]
fn reads_cwd_from_session_with_leading_blank_lines() {
    let tmp = TempDir::new();
    let stored_cwd = tmp.path().join("stored-project");
    let file = tmp.path().join("header.jsonl");
    std::fs::write(
        &file,
        format!(
            "\n  \n{}\n",
            header_line("leading-blank", &stored_cwd.to_string_lossy())
        ),
    )
    .expect("write");
    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    assert_eq!(sm.get_session_id(), "leading-blank");
    assert_eq!(sm.get_cwd(), stored_cwd);
}

#[test]
fn reads_cwd_from_session_with_leading_malformed_lines() {
    let tmp = TempDir::new();
    let stored_cwd = tmp.path().join("stored-project");
    let file = tmp.path().join("header.jsonl");
    std::fs::write(
        &file,
        format!(
            "not json\n{{broken json\n{}\n",
            header_line("leading-malformed", &stored_cwd.to_string_lossy())
        ),
    )
    .expect("write");
    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    assert_eq!(sm.get_session_id(), "leading-malformed");
    assert_eq!(sm.get_cwd(), stored_cwd);
}

#[test]
fn reads_cwd_from_session_with_multi_buffer_header() {
    let tmp = TempDir::new();
    let stored_cwd = tmp.path().join("stored-project");
    let file = tmp.path().join("header.jsonl");
    std::fs::write(
        &file,
        format!(
            "{}\n",
            header_line(&"a".repeat(8192), &stored_cwd.to_string_lossy())
        ),
    )
    .expect("write");
    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    assert_eq!(sm.get_session_id(), "a".repeat(8192));
    assert_eq!(sm.get_cwd(), stored_cwd);
}

/// Oversized header falls back to full loading (T07 self-check: read robustness).
#[test]
fn opens_compatible_sessions_beyond_the_discovery_scan_limit() {
    let tmp = TempDir::new();
    let stored_cwd = tmp.path().join("stored-project");
    let override_cwd = tmp.path().join("override-project");

    let cases: Vec<(String, String)> = vec![
        (
            "large-header".to_owned(),
            "a".repeat(MAX_SESSION_HEADER_SCAN_BYTES + 1),
        ),
        ("large-prefix".to_owned(), "large-prefix".to_owned()),
    ];
    for (name, id) in cases {
        let file = tmp.path().join(format!("{name}.jsonl"));
        let prefix = if name == "large-prefix" {
            format!("{}\n", "x".repeat(MAX_SESSION_HEADER_SCAN_BYTES + 1))
        } else {
            String::new()
        };
        std::fs::write(
            &file,
            format!(
                "{prefix}{}\n",
                header_line(&id, &stored_cwd.to_string_lossy())
            ),
        )
        .expect("write");
        for cwd_override in [None, Some(override_cwd.as_path())] {
            let sm = SessionManager::open(&file, Some(tmp.path()), cwd_override).expect("open");
            assert_eq!(sm.get_session_id(), id);
            assert_eq!(sm.get_cwd(), cwd_override.unwrap_or(&stored_cwd));
        }
    }
}

#[test]
fn find_most_recent_session_returns_none_for_empty_directory() {
    let tmp = TempDir::new();
    assert_eq!(find_most_recent_session(tmp.path(), None), None);
}

#[test]
fn find_most_recent_session_returns_none_for_nonexistent_directory() {
    let tmp = TempDir::new();
    assert_eq!(
        find_most_recent_session(&tmp.path().join("nonexistent"), None),
        None
    );
}

#[test]
fn find_most_recent_session_ignores_non_jsonl_files() {
    let tmp = TempDir::new();
    std::fs::write(tmp.path().join("file.txt"), "hello").expect("write");
    std::fs::write(tmp.path().join("file.json"), "{}").expect("write");
    assert_eq!(find_most_recent_session(tmp.path(), None), None);
}

#[test]
fn find_most_recent_session_ignores_jsonl_without_valid_header() {
    let tmp = TempDir::new();
    std::fs::write(tmp.path().join("invalid.jsonl"), "{\"type\":\"message\"}\n").expect("write");
    assert_eq!(find_most_recent_session(tmp.path(), None), None);
}

#[test]
fn find_most_recent_session_returns_single_valid_file() {
    let tmp = TempDir::new();
    let file = tmp.path().join("session.jsonl");
    std::fs::write(&file, format!("{}\n", header_line("abc", "/tmp"))).expect("write");
    assert_eq!(find_most_recent_session(tmp.path(), None), Some(file));
}

#[test]
fn find_most_recent_session_returns_most_recently_modified() {
    let tmp = TempDir::new();
    let older = tmp.path().join("older.jsonl");
    let newer = tmp.path().join("newer.jsonl");
    std::fs::write(&older, format!("{}\n", header_line("old", "/tmp"))).expect("write");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&newer, format!("{}\n", header_line("new", "/tmp"))).expect("write");
    assert_eq!(find_most_recent_session(tmp.path(), None), Some(newer));
}

#[test]
fn find_most_recent_session_skips_invalid_and_returns_valid() {
    let tmp = TempDir::new();
    std::fs::write(
        tmp.path().join("invalid.jsonl"),
        "{\"type\":\"not-session\"}\n",
    )
    .expect("write");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let valid = tmp.path().join("valid.jsonl");
    std::fs::write(&valid, format!("{}\n", header_line("abc", "/tmp"))).expect("write");
    assert_eq!(find_most_recent_session(tmp.path(), None), Some(valid));
}

#[test]
fn find_most_recent_session_skips_oversized_corrupt_files() {
    let tmp = TempDir::new();
    std::fs::write(
        tmp.path().join("oversized.jsonl"),
        "x".repeat(MAX_SESSION_HEADER_SCAN_BYTES + 1),
    )
    .expect("write");
    let valid = tmp.path().join("valid.jsonl");
    std::fs::write(&valid, format!("{}\n", header_line("abc", "/tmp"))).expect("write");
    assert_eq!(find_most_recent_session(tmp.path(), None), Some(valid));
}

#[test]
fn find_most_recent_session_filters_by_cwd() {
    let tmp = TempDir::new();
    let project_a = tmp.path().join("project-a");
    let project_b = tmp.path().join("project-b");
    let file_a = tmp.path().join("a.jsonl");
    let file_b = tmp.path().join("b.jsonl");
    std::fs::write(
        &file_a,
        format!("{}\n", header_line("a", &project_a.to_string_lossy())),
    )
    .expect("write");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        &file_b,
        format!("{}\n", header_line("b", &project_b.to_string_lossy())),
    )
    .expect("write");
    assert_eq!(
        find_most_recent_session(tmp.path(), Some(&project_a)),
        Some(file_a)
    );
    assert_eq!(
        find_most_recent_session(tmp.path(), Some(&project_b)),
        Some(file_b)
    );
}

#[test]
fn set_session_file_truncates_and_rewrites_empty_file_with_valid_header() {
    let tmp = TempDir::new();
    let file = tmp.path().join("empty.jsonl");
    std::fs::write(&file, "").expect("write");

    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    assert!(!sm.get_session_id().is_empty());
    assert!(sm.get_header().is_some());

    let content = std::fs::read_to_string(&file).expect("read");
    let lines: Vec<&str> = content
        .trim()
        .split('\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 1);
    let header: Value = serde_json::from_str(lines[0]).expect("json");
    assert_eq!(header["type"], json!("session"));
    assert_eq!(header["id"], json!(sm.get_session_id()));
}

#[test]
fn set_session_file_throws_and_preserves_non_empty_file_without_valid_header() {
    let tmp = TempDir::new();
    let file = tmp.path().join("no-header.jsonl");
    let original = "{\"type\":\"message\",\"id\":\"abc\",\"parentId\":\"orphaned\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"message\":{\"role\":\"assistant\",\"content\":\"test\"}}\n";
    std::fs::write(&file, original).expect("write");

    let err = SessionManager::open(&file, Some(tmp.path()), None).expect_err("must fail");
    assert_eq!(
        err.to_string(),
        format!(
            "session error: Session file is not a valid pi session: {}",
            file.display()
        )
    );
    assert_eq!(std::fs::read_to_string(&file).expect("read"), original);
}

#[test]
fn set_session_file_throws_and_preserves_non_session_jsonl_files() {
    let tmp = TempDir::new();
    let file = tmp.path().join("not-a-session.log");
    let original = "{\"type\":\"event\",\"data\":\"not a session\"}\n";
    std::fs::write(&file, original).expect("write");

    let err = SessionManager::open(&file, Some(tmp.path()), None).expect_err("must fail");
    assert!(err.to_string().contains("not a valid pi session"));
    assert_eq!(std::fs::read_to_string(&file).expect("read"), original);
}

#[test]
fn set_session_file_preserves_explicit_path_when_recovering_from_corrupted_file() {
    let tmp = TempDir::new();
    let file = tmp.path().join("my-session.jsonl");
    std::fs::write(&file, "").expect("write");
    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    assert_eq!(sm.get_session_file(), Some(file.as_path()));
}

#[test]
fn subsequent_loads_of_initialized_empty_file_work() {
    let tmp = TempDir::new();
    let file = tmp.path().join("empty.jsonl");
    std::fs::write(&file, "").expect("write");
    let sm1 = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    let id = sm1.get_session_id().to_owned();
    let sm2 = SessionManager::open(&file, Some(tmp.path()), None).expect("reopen");
    assert_eq!(sm2.get_session_id(), id);
    assert!(sm2.get_header().is_some());
}

// ===========================================================================
// tree-traversal.test.ts — append operations
// ===========================================================================

#[test]
fn append_message_creates_entry_with_correct_parent_id_chain() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("first")).expect("append");
    let id2 = session
        .append_message(assistant_msg("second"))
        .expect("append");
    let id3 = session.append_message(user_msg("third")).expect("append");

    let entries = session.get_entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].id(), id1);
    assert_eq!(entries[0].parent_id(), None);
    assert_eq!(entries[0].type_tag(), "message");
    assert_eq!(entries[1].id(), id2);
    assert_eq!(entries[1].parent_id(), Some(id1.as_str()));
    assert_eq!(entries[2].id(), id3);
    assert_eq!(entries[2].parent_id(), Some(id2.as_str()));
}

#[test]
fn append_thinking_level_change_integrates_into_tree() {
    let mut session = in_memory();
    let msg_id = session.append_message(user_msg("hello")).expect("append");
    let thinking_id = session
        .append_thinking_level_change("high")
        .expect("append");
    session
        .append_message(assistant_msg("response"))
        .expect("append");

    let entries = session.get_entries();
    assert_eq!(entries.len(), 3);
    let thinking = entries
        .iter()
        .find(|e| e.type_tag() == "thinking_level_change")
        .expect("thinking entry");
    assert_eq!(thinking.id(), thinking_id);
    assert_eq!(thinking.parent_id(), Some(msg_id.as_str()));
    assert_eq!(entries[2].parent_id(), Some(thinking_id.as_str()));
}

#[test]
fn append_model_change_integrates_into_tree() {
    let mut session = in_memory();
    let msg_id = session.append_message(user_msg("hello")).expect("append");
    let model_id = session
        .append_model_change("openai", "gpt-4")
        .expect("append");
    session
        .append_message(assistant_msg("response"))
        .expect("append");

    let entries = session.get_entries();
    let model = entries
        .iter()
        .find(|e| e.type_tag() == "model_change")
        .expect("model entry");
    assert_eq!(model.id(), model_id);
    assert_eq!(model.parent_id(), Some(msg_id.as_str()));
    match model.known() {
        Some(SessionEntry::ModelChange(m)) => {
            assert_eq!(m.provider, "openai");
            assert_eq!(m.model_id, "gpt-4");
        }
        other => panic!("expected model_change, got {other:?}"),
    }
    assert_eq!(entries[2].parent_id(), Some(model_id.as_str()));
}

#[test]
fn append_compaction_integrates_into_tree() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    let usage = usage_sample();
    let compaction_id = session
        .append_compaction(
            "summary",
            &id1,
            1000,
            None,
            Some(false),
            Some(usage.clone()),
        )
        .expect("append");
    session.append_message(user_msg("3")).expect("append");

    let entries = session.get_entries();
    let compaction = entries
        .iter()
        .find(|e| e.type_tag() == "compaction")
        .expect("compaction entry");
    assert_eq!(compaction.id(), compaction_id);
    assert_eq!(compaction.parent_id(), Some(id2.as_str()));
    match compaction.known() {
        Some(SessionEntry::Compaction(c)) => {
            assert_eq!(c.summary, "summary");
            assert_eq!(c.first_kept_entry_id.as_deref(), Some(id1.as_str()));
            assert_eq!(c.tokens_before, 1000);
            assert_eq!(c.usage, Some(usage));
        }
        other => panic!("expected compaction, got {other:?}"),
    }
    assert_eq!(entries[3].parent_id(), Some(compaction_id.as_str()));
}

#[test]
fn append_custom_entry_integrates_into_tree() {
    let mut session = in_memory();
    let msg_id = session.append_message(user_msg("hello")).expect("append");
    let custom_id = session
        .append_custom_entry("my_data", Some(json!({"key": "value"})))
        .expect("append");
    session
        .append_message(assistant_msg("response"))
        .expect("append");

    let entries = session.get_entries();
    let custom = entries
        .iter()
        .find(|e| e.type_tag() == "custom")
        .expect("custom entry");
    assert_eq!(custom.id(), custom_id);
    assert_eq!(custom.parent_id(), Some(msg_id.as_str()));
    match custom.known() {
        Some(SessionEntry::Custom(c)) => {
            assert_eq!(c.custom_type, "my_data");
            assert_eq!(c.data, Some(json!({"key": "value"})));
        }
        other => panic!("expected custom, got {other:?}"),
    }
    assert_eq!(entries[2].parent_id(), Some(custom_id.as_str()));
}

#[test]
fn leaf_pointer_advances_after_each_append() {
    let mut session = in_memory();
    assert_eq!(session.get_leaf_id(), None);

    let id1 = session.append_message(user_msg("1")).expect("append");
    assert_eq!(session.get_leaf_id(), Some(id1.as_str()));
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    assert_eq!(session.get_leaf_id(), Some(id2.as_str()));
    let id3 = session
        .append_thinking_level_change("high")
        .expect("append");
    assert_eq!(session.get_leaf_id(), Some(id3.as_str()));
}

/// Harness `leaf` records replay with the harness leaf semantics — the leaf
/// moves to the record's `targetId` (`null` clears it) rather than to the
/// record's own id (`leafIdAfterEntry`, jsonl-storage.ts:134-136; T16 interop
/// alignment, see module header). The main path never writes leaf records, so
/// this only affects harness-produced files.
#[test]
fn leaf_records_replay_with_harness_target_semantics() {
    let tmp = TempDir::new();
    let file = tmp.path().join("harness.jsonl");
    // Trailing leaf move: `targetId` wins over the leaf record's own id.
    let content = format!(
        "{}\n{}\n{}\n{}\n",
        header_line("sess-1", "/tmp"),
        user_line("u1", "", "one"),
        user_line("u2", "u1", "two"),
        "{\"type\":\"leaf\",\"id\":\"l1\",\"parentId\":\"u2\",\"timestamp\":\"2025-01-01T00:00:03.000Z\",\"targetId\":\"u1\"}",
    );
    std::fs::write(&file, content).expect("write harness file");
    let mut sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    assert_eq!(sm.get_leaf_id(), Some("u1"));
    assert_eq!(sm.get_leaf_entry().expect("leaf entry").id(), "u1");

    // Continuing parents the next message to the target, not to the leaf record.
    let id = sm.append_message(user_msg("three")).expect("append");
    let appended = sm.get_entry(&id).expect("entry");
    assert_eq!(appended.parent_id(), Some("u1"));
    assert_eq!(sm.get_leaf_id(), Some(id.as_str()));

    // `targetId: null` clears the leaf (harness moveTo(root)).
    let null_file = tmp.path().join("harness-null.jsonl");
    let content = format!(
        "{}\n{}\n{}\n",
        header_line("sess-2", "/tmp"),
        user_line("u1", "", "one"),
        "{\"type\":\"leaf\",\"id\":\"l1\",\"parentId\":\"u1\",\"timestamp\":\"2025-01-01T00:00:03.000Z\",\"targetId\":null}",
    );
    std::fs::write(&null_file, content).expect("write harness file");
    let sm = SessionManager::open(&null_file, Some(tmp.path()), None).expect("open");
    assert_eq!(sm.get_leaf_id(), None);
    assert!(sm.get_branch(None).is_empty());
}

// ===========================================================================
// tree-traversal.test.ts — getBranch
// ===========================================================================

#[test]
fn get_branch_returns_empty_array_for_empty_session() {
    let session = in_memory();
    assert!(session.get_branch(None).is_empty());
}

#[test]
fn get_branch_returns_single_entry_path() {
    let mut session = in_memory();
    let id = session.append_message(user_msg("hello")).expect("append");
    let path = session.get_branch(None);
    assert_eq!(entry_ids(&path), [id.as_str()]);
}

#[test]
fn get_branch_returns_full_path_from_root_to_leaf() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    let id3 = session
        .append_thinking_level_change("high")
        .expect("append");
    let id4 = session.append_message(user_msg("3")).expect("append");
    let path = session.get_branch(None);
    assert_eq!(entry_ids(&path), [id1, id2, id3, id4]);
}

#[test]
fn get_branch_returns_path_from_specified_entry_to_root() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    session.append_message(user_msg("3")).expect("append");
    session.append_message(assistant_msg("4")).expect("append");
    let path = session.get_branch(Some(&id2));
    assert_eq!(entry_ids(&path), [id1, id2]);
}

// ===========================================================================
// tree-traversal.test.ts — getTree
// ===========================================================================

#[test]
fn get_tree_returns_empty_array_for_empty_session() {
    let session = in_memory();
    assert!(session.get_tree().is_empty());
}

#[test]
fn get_tree_returns_single_root_for_linear_session() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    let id3 = session.append_message(user_msg("3")).expect("append");

    let tree = session.get_tree();
    assert_eq!(tree.len(), 1);
    let root = &tree[0];
    assert_eq!(root.entry.id(), id1);
    assert_eq!(root.children.len(), 1);
    assert_eq!(root.children[0].entry.id(), id2);
    assert_eq!(root.children[0].children.len(), 1);
    assert_eq!(root.children[0].children[0].entry.id(), id3);
    assert!(root.children[0].children[0].children.is_empty());
}

#[test]
fn get_tree_returns_tree_with_branches_after_branch() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    let id3 = session.append_message(user_msg("3")).expect("append");

    session.branch(&id2).expect("branch");
    let id4 = session
        .append_message(user_msg("4-branch"))
        .expect("append");

    let tree = session.get_tree();
    assert_eq!(tree.len(), 1);
    let root = &tree[0];
    assert_eq!(root.entry.id(), id1);
    let node2 = &root.children[0];
    assert_eq!(node2.entry.id(), id2);
    assert_eq!(node2.children.len(), 2);
    let mut child_ids: Vec<&str> = node2.children.iter().map(|c| c.entry.id()).collect();
    child_ids.sort_unstable();
    let mut expected = vec![id3, id4];
    expected.sort_unstable();
    assert_eq!(child_ids, expected);
}

#[test]
fn get_tree_handles_multiple_branches_at_same_point() {
    let mut session = in_memory();
    session.append_message(user_msg("root")).expect("append");
    let id2 = session
        .append_message(assistant_msg("response"))
        .expect("append");

    session.branch(&id2).expect("branch");
    let id_a = session
        .append_message(user_msg("branch-A"))
        .expect("append");
    session.branch(&id2).expect("branch");
    let id_b = session
        .append_message(user_msg("branch-B"))
        .expect("append");
    session.branch(&id2).expect("branch");
    let id_c = session
        .append_message(user_msg("branch-C"))
        .expect("append");

    let tree = session.get_tree();
    let node2 = &tree[0].children[0];
    assert_eq!(node2.entry.id(), id2);
    assert_eq!(node2.children.len(), 3);
    let mut branch_ids: Vec<&str> = node2.children.iter().map(|c| c.entry.id()).collect();
    branch_ids.sort_unstable();
    let mut expected = vec![id_a, id_b, id_c];
    expected.sort_unstable();
    assert_eq!(branch_ids, expected);
}

#[test]
fn get_tree_handles_deep_branching() {
    let mut session = in_memory();
    session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    let id3 = session.append_message(user_msg("3")).expect("append");
    session.append_message(assistant_msg("4")).expect("append");

    session.branch(&id2).expect("branch");
    let id5 = session.append_message(user_msg("5")).expect("append");
    session.append_message(assistant_msg("6")).expect("append");
    session.branch(&id5).expect("branch");
    session.append_message(user_msg("7")).expect("append");

    let tree = session.get_tree();
    let node2 = &tree[0].children[0];
    assert_eq!(node2.children.len(), 2);
    let node5 = node2
        .children
        .iter()
        .find(|c| c.entry.id() == id5)
        .expect("node5");
    assert_eq!(node5.children.len(), 2);
    let node3 = node2
        .children
        .iter()
        .find(|c| c.entry.id() == id3)
        .expect("node3");
    assert_eq!(node3.children.len(), 1);
}

/// getTree: orphans as roots + children sorted by timestamp (T07 self-check).
#[test]
fn get_tree_treats_orphans_as_roots_and_sorts_children_by_timestamp() {
    // Hand-built file: entry "b" and "a" both children of "root" with
    // out-of-order timestamps; "orphan" has a missing parent.
    let tmp = TempDir::new();
    let file = tmp.path().join("tree.jsonl");
    let content = [
        header_line("s1", "/tmp"),
        "{\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"root\",\"timestamp\":1}}".to_owned(),
        "{\"type\":\"message\",\"id\":\"b\",\"parentId\":\"root\",\"timestamp\":\"2025-01-01T00:00:02.000Z\",\"message\":{\"role\":\"user\",\"content\":\"b\",\"timestamp\":2}}".to_owned(),
        "{\"type\":\"message\",\"id\":\"a\",\"parentId\":\"root\",\"timestamp\":\"2025-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"a\",\"timestamp\":3}}".to_owned(),
        "{\"type\":\"message\",\"id\":\"orphan\",\"parentId\":\"missing\",\"timestamp\":\"2025-01-01T00:00:03.000Z\",\"message\":{\"role\":\"user\",\"content\":\"o\",\"timestamp\":4}}".to_owned(),
    ];
    std::fs::write(&file, content.join("\n") + "\n").expect("write");
    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");

    let tree = sm.get_tree();
    assert_eq!(tree.len(), 2, "orphan becomes a second root");
    let root = tree.iter().find(|n| n.entry.id() == "root").expect("root");
    let child_ids: Vec<&str> = root.children.iter().map(|c| c.entry.id()).collect();
    assert_eq!(child_ids, ["a", "b"], "children sorted by timestamp");
    assert!(tree.iter().any(|n| n.entry.id() == "orphan"));
}

// ===========================================================================
// tree-traversal.test.ts — branch / branchWithSummary / getLeafEntry / getEntry
// ===========================================================================

#[test]
fn branch_moves_leaf_pointer_to_specified_entry() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    session.append_message(assistant_msg("2")).expect("append");
    let id3 = session.append_message(user_msg("3")).expect("append");
    assert_eq!(session.get_leaf_id(), Some(id3.as_str()));
    session.branch(&id1).expect("branch");
    assert_eq!(session.get_leaf_id(), Some(id1.as_str()));
}

#[test]
fn branch_throws_for_nonexistent_entry() {
    let mut session = in_memory();
    session.append_message(user_msg("hello")).expect("append");
    let err = session.branch("nonexistent").expect_err("must fail");
    assert_eq!(
        err.to_string(),
        "session error: Entry nonexistent not found"
    );
}

#[test]
fn branch_new_appends_become_children_of_branch_point() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    session.append_message(assistant_msg("2")).expect("append");
    session.branch(&id1).expect("branch");
    let id3 = session
        .append_message(user_msg("branched"))
        .expect("append");
    let branched = session.get_entry(&id3).expect("entry");
    assert_eq!(branched.parent_id(), Some(id1.as_str()));
}

#[test]
fn branch_with_summary_inserts_branch_summary_and_advances_leaf() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    session.append_message(assistant_msg("2")).expect("append");
    session.append_message(user_msg("3")).expect("append");

    let usage = usage_sample();
    let summary_id = session
        .branch_with_summary(
            Some(&id1),
            "Summary of abandoned work",
            None,
            Some(false),
            Some(usage.clone()),
        )
        .expect("branch_with_summary");
    assert_eq!(session.get_leaf_id(), Some(summary_id.as_str()));

    let entries = session.get_entries();
    let summary = entries
        .iter()
        .find(|e| e.type_tag() == "branch_summary")
        .expect("summary entry");
    assert_eq!(summary.parent_id(), Some(id1.as_str()));
    match summary.known() {
        Some(SessionEntry::BranchSummary(b)) => {
            assert_eq!(b.summary, "Summary of abandoned work");
            assert_eq!(b.usage, Some(usage));
        }
        other => panic!("expected branch_summary, got {other:?}"),
    }
}

#[test]
fn branch_with_summary_throws_for_nonexistent_entry() {
    let mut session = in_memory();
    session.append_message(user_msg("hello")).expect("append");
    let err = session
        .branch_with_summary(Some("nonexistent"), "summary", None, None, None)
        .expect_err("must fail");
    assert_eq!(
        err.to_string(),
        "session error: Entry nonexistent not found"
    );
}

#[test]
fn get_leaf_entry_returns_none_for_empty_session() {
    let session = in_memory();
    assert!(session.get_leaf_entry().is_none());
}

#[test]
fn get_leaf_entry_returns_current_leaf_entry() {
    let mut session = in_memory();
    session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    let leaf = session.get_leaf_entry().expect("leaf");
    assert_eq!(leaf.id(), id2);
}

#[test]
fn get_entry_returns_none_for_nonexistent_id() {
    let session = in_memory();
    assert!(session.get_entry("nonexistent").is_none());
}

#[test]
fn get_entry_returns_entry_by_id() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("first")).expect("append");
    let id2 = session
        .append_message(assistant_msg("second"))
        .expect("append");

    let entry1 = session.get_entry(&id1).expect("entry1");
    assert_eq!(entry1.type_tag(), "message");
    match entry1.known() {
        Some(SessionEntry::Message(m)) => assert_eq!(msg_text(&m.message), "first"),
        other => panic!("expected message, got {other:?}"),
    }
    let entry2 = session.get_entry(&id2).expect("entry2");
    match entry2.known() {
        Some(SessionEntry::Message(m)) => assert_eq!(msg_text(&m.message), "second"),
        other => panic!("expected message, got {other:?}"),
    }
}

#[test]
fn build_session_context_returns_messages_from_current_branch_only() {
    let mut session = in_memory();
    session.append_message(user_msg("msg1")).expect("append");
    let id2 = session
        .append_message(assistant_msg("msg2"))
        .expect("append");
    session.append_message(user_msg("msg3")).expect("append");

    session.branch(&id2).expect("branch");
    session
        .append_message(assistant_msg("msg4-branch"))
        .expect("append");

    let ctx = session.build_session_context();
    assert_eq!(ctx.messages.len(), 3);
    assert_eq!(msg_text(&ctx.messages[0]), "msg1");
    assert_eq!(msg_text(&ctx.messages[1]), "msg2");
    assert_eq!(msg_text(&ctx.messages[2]), "msg4-branch");
}

// ===========================================================================
// tree-traversal.test.ts — createBranchedSession
// ===========================================================================

#[test]
fn create_branched_session_throws_for_nonexistent_entry() {
    let mut session = in_memory();
    session.append_message(user_msg("hello")).expect("append");
    let err = session
        .create_branched_session("nonexistent")
        .expect_err("must fail");
    assert_eq!(
        err.to_string(),
        "session error: Entry nonexistent not found"
    );
}

#[test]
fn create_branched_session_creates_new_session_with_path_to_specified_leaf_in_memory() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    let id3 = session.append_message(user_msg("3")).expect("append");
    session.append_message(assistant_msg("4")).expect("append");

    session.branch(&id3).expect("branch");
    session.append_message(user_msg("5")).expect("append");

    let result = session
        .create_branched_session(&id2)
        .expect("create_branched_session");
    assert_eq!(result, None, "in-memory returns None");

    let entries = session.get_entries();
    assert_eq!(entry_ids(&entries), [id1, id2]);
}

#[test]
fn create_branched_session_extracts_correct_path_from_branched_tree() {
    let mut session = in_memory();
    let id1 = session.append_message(user_msg("1")).expect("append");
    let id2 = session.append_message(assistant_msg("2")).expect("append");
    session.append_message(user_msg("3")).expect("append");

    session.branch(&id2).expect("branch");
    let id4 = session.append_message(user_msg("4")).expect("append");
    let id5 = session.append_message(assistant_msg("5")).expect("append");

    session
        .create_branched_session(&id5)
        .expect("create_branched_session");
    let entries = session.get_entries();
    assert_eq!(entry_ids(&entries), [id1, id2, id4, id5]);
}

#[test]
fn create_branched_session_does_not_duplicate_entries_when_forking_from_first_user_message() {
    let tmp = TempDir::new();
    let mut session =
        SessionManager::create(tmp.path(), Some(tmp.path()), NewSessionOptions::default())
            .expect("create");
    let id1 = session
        .append_message(user_msg("first question"))
        .expect("append");
    session
        .append_message(assistant_msg("first answer"))
        .expect("append");
    session
        .append_message(user_msg("second question"))
        .expect("append");
    session
        .append_message(assistant_msg("second answer"))
        .expect("append");

    // Fork from the very first user message (no assistant in branched path).
    let new_file = session
        .create_branched_session(&id1)
        .expect("create_branched_session")
        .expect("persisted session returns a file");

    // No assistant in path: file deferred until first assistant response.
    assert!(!new_file.exists());

    // Simulate extension adding an entry before the assistant.
    session
        .append_custom_entry("preset-state", Some(json!({"name": "plan"})))
        .expect("append");
    session
        .append_message(assistant_msg("new answer"))
        .expect("append");

    // Exactly one header and no duplicate ids.
    assert!(new_file.exists());
    let content = std::fs::read_to_string(&new_file).expect("read");
    let records: Vec<Value> = content
        .trim()
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    assert_eq!(
        records
            .iter()
            .filter(|r| r["type"] == json!("session"))
            .count(),
        1
    );
    let ids: Vec<&str> = records
        .iter()
        .filter(|r| r["type"] != json!("session"))
        .filter_map(|r| r["id"].as_str())
        .collect();
    let unique: std::collections::HashSet<&&str> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());
}

#[test]
fn create_branched_session_writes_file_immediately_when_forking_from_point_with_assistant() {
    let tmp = TempDir::new();
    let mut session =
        SessionManager::create(tmp.path(), Some(tmp.path()), NewSessionOptions::default())
            .expect("create");
    session
        .append_message(user_msg("first question"))
        .expect("append");
    let id2 = session
        .append_message(assistant_msg("first answer"))
        .expect("append");
    session
        .append_message(user_msg("second question"))
        .expect("append");
    session
        .append_message(assistant_msg("second answer"))
        .expect("append");

    let new_file = session
        .create_branched_session(&id2)
        .expect("create_branched_session")
        .expect("file");
    assert!(new_file.exists());
    let content = std::fs::read_to_string(&new_file).expect("read");
    let header_count = content
        .trim()
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("json"))
        .filter(|r| r["type"] == json!("session"))
        .count();
    assert_eq!(header_count, 1);
}

#[test]
fn preserves_tool_and_summary_usage_across_a_file_backed_reload() {
    let tmp = TempDir::new();
    let mut session =
        SessionManager::create(tmp.path(), Some(tmp.path()), NewSessionOptions::default())
            .expect("create");
    let root_id = session
        .append_message(user_msg("question"))
        .expect("append");
    session
        .append_message(assistant_msg("answer"))
        .expect("append");
    let usage = usage_sample();
    session
        .append_message(AgentMessage::ToolResult(rpi_ai::types::ToolResultMessage {
            role: rpi_ai::types::ToolResultRole::ToolResult,
            tool_call_id: "call-1".to_owned(),
            tool_name: "nested-model".to_owned(),
            content: vec![rpi_ai::types::ToolResultContent::Text(TextContent {
                text: "result".to_owned(),
                text_signature: None,
            })],
            details: None,
            usage: Some(usage.clone()),
            added_tool_names: None,
            is_error: false,
            timestamp: 1,
        }))
        .expect("append");
    session
        .append_compaction(
            "summary",
            &root_id,
            100,
            None,
            Some(false),
            Some(usage.clone()),
        )
        .expect("append");
    session
        .branch_with_summary(
            Some(&root_id),
            "branch summary",
            None,
            Some(false),
            Some(usage.clone()),
        )
        .expect("append");

    let file = session.get_session_file().expect("file").to_path_buf();
    let reopened = SessionManager::open(&file, Some(tmp.path()), None).expect("reopen");
    let entries = reopened.get_entries();

    let compaction_usage = entries.iter().find_map(|e| match e.known() {
        Some(SessionEntry::Compaction(c)) => c.usage.clone(),
        _ => None,
    });
    assert_eq!(compaction_usage, Some(usage.clone()));
    let branch_usage = entries.iter().find_map(|e| match e.known() {
        Some(SessionEntry::BranchSummary(b)) => b.usage.clone(),
        _ => None,
    });
    assert_eq!(branch_usage, Some(usage.clone()));
    let tool_usage = entries.iter().find_map(|e| match e.known() {
        Some(SessionEntry::Message(m)) => match &m.message {
            AgentMessage::ToolResult(t) => t.usage.clone(),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(tool_usage, Some(usage));
}

// ===========================================================================
// labels.test.ts
// ===========================================================================

#[test]
fn sets_and_gets_labels() {
    let mut session = in_memory();
    let msg_id = session.append_message(user_msg("hello")).expect("append");
    assert_eq!(session.get_label(&msg_id), None);

    let label_id = session
        .append_label_change(&msg_id, Some("checkpoint"))
        .expect("label");
    assert_eq!(session.get_label(&msg_id), Some("checkpoint"));

    let entries = session.get_entries();
    let label = entries
        .iter()
        .find(|e| e.type_tag() == "label")
        .expect("label entry");
    assert_eq!(label.id(), label_id);
    match label.known() {
        Some(SessionEntry::Label(l)) => {
            assert_eq!(l.target_id, msg_id);
            assert_eq!(l.label.as_deref(), Some("checkpoint"));
        }
        other => panic!("expected label, got {other:?}"),
    }
}

#[test]
fn clears_labels_with_none() {
    let mut session = in_memory();
    let msg_id = session.append_message(user_msg("hello")).expect("append");
    session
        .append_label_change(&msg_id, Some("checkpoint"))
        .expect("label");
    assert_eq!(session.get_label(&msg_id), Some("checkpoint"));
    session.append_label_change(&msg_id, None).expect("clear");
    assert_eq!(session.get_label(&msg_id), None);
}

#[test]
fn last_label_wins() {
    let mut session = in_memory();
    let msg_id = session.append_message(user_msg("hello")).expect("append");
    session
        .append_label_change(&msg_id, Some("first"))
        .expect("label");
    session
        .append_label_change(&msg_id, Some("second"))
        .expect("label");
    let last_id = session
        .append_label_change(&msg_id, Some("third"))
        .expect("label");
    assert_eq!(session.get_label(&msg_id), Some("third"));

    let entries = session.get_entries();
    let last = entries
        .iter()
        .find(|e| e.id() == last_id)
        .expect("last label");
    let tree = session.get_tree();
    let node = tree.iter().find(|n| n.entry.id() == msg_id).expect("node");
    assert_eq!(node.label_timestamp.as_deref(), Some(last.timestamp()));
}

#[test]
fn labels_are_included_in_tree_nodes() {
    let mut session = in_memory();
    let msg1_id = session.append_message(user_msg("hello")).expect("append");
    let msg2_id = session.append_message(assistant_msg("hi")).expect("append");
    let label1_id = session
        .append_label_change(&msg1_id, Some("start"))
        .expect("label");
    let label2_id = session
        .append_label_change(&msg2_id, Some("response"))
        .expect("label");

    let entries = session.get_entries();
    let label1_ts = entries
        .iter()
        .find(|e| e.id() == label1_id)
        .expect("l1")
        .timestamp()
        .to_owned();
    let label2_ts = entries
        .iter()
        .find(|e| e.id() == label2_id)
        .expect("l2")
        .timestamp()
        .to_owned();

    let tree = session.get_tree();
    let node1 = tree
        .iter()
        .find(|n| n.entry.id() == msg1_id)
        .expect("node1");
    assert_eq!(node1.label.as_deref(), Some("start"));
    assert_eq!(node1.label_timestamp.as_deref(), Some(label1_ts.as_str()));
    let node2 = node1
        .children
        .iter()
        .find(|n| n.entry.id() == msg2_id)
        .expect("node2");
    assert_eq!(node2.label.as_deref(), Some("response"));
    assert_eq!(node2.label_timestamp.as_deref(), Some(label2_ts.as_str()));
}

#[test]
fn labels_are_preserved_in_create_branched_session() {
    let mut session = in_memory();
    let msg1_id = session.append_message(user_msg("hello")).expect("append");
    let msg2_id = session.append_message(assistant_msg("hi")).expect("append");
    let label1_id = session
        .append_label_change(&msg1_id, Some("important"))
        .expect("label");
    let label2_id = session
        .append_label_change(&msg2_id, Some("also-important"))
        .expect("label");

    let entries_before = session.get_entries();
    let label1_ts = entries_before
        .iter()
        .find(|e| e.id() == label1_id)
        .expect("l1")
        .timestamp()
        .to_owned();
    let label2_ts = entries_before
        .iter()
        .find(|e| e.id() == label2_id)
        .expect("l2")
        .timestamp()
        .to_owned();

    session
        .create_branched_session(&msg2_id)
        .expect("create_branched_session");

    assert_eq!(session.get_label(&msg1_id), Some("important"));
    assert_eq!(session.get_label(&msg2_id), Some("also-important"));

    let entries = session.get_entries();
    let label_entries: Vec<&StoredEntry> =
        entries.iter().filter(|e| e.type_tag() == "label").collect();
    assert_eq!(label_entries.len(), 2);

    let tree = session.get_tree();
    let node1 = tree
        .iter()
        .find(|n| n.entry.id() == msg1_id)
        .expect("node1");
    let node2 = node1
        .children
        .iter()
        .find(|n| n.entry.id() == msg2_id)
        .expect("node2");
    assert_eq!(node1.label_timestamp.as_deref(), Some(label1_ts.as_str()));
    assert_eq!(node2.label_timestamp.as_deref(), Some(label2_ts.as_str()));
}

#[test]
fn rewires_children_of_removed_labels_when_forking() {
    let mut session = in_memory();
    let msg1_id = session.append_message(user_msg("hello")).expect("append");
    session
        .append_label_change(&msg1_id, Some("checkpoint"))
        .expect("label");
    let model_change_id = session
        .append_model_change("anthropic", "claude-test")
        .expect("append");
    let msg2_id = session
        .append_message(user_msg("followup"))
        .expect("append");

    session
        .create_branched_session(&msg2_id)
        .expect("create_branched_session");
    assert_eq!(
        session
            .get_entry(&model_change_id)
            .expect("entry")
            .parent_id(),
        Some(msg1_id.as_str())
    );
}

#[test]
fn labels_not_on_path_are_not_preserved_in_create_branched_session() {
    let mut session = in_memory();
    let msg1_id = session.append_message(user_msg("hello")).expect("append");
    let msg2_id = session.append_message(assistant_msg("hi")).expect("append");
    let msg3_id = session
        .append_message(user_msg("followup"))
        .expect("append");
    session
        .append_label_change(&msg1_id, Some("first"))
        .expect("label");
    session
        .append_label_change(&msg2_id, Some("second"))
        .expect("label");
    session
        .append_label_change(&msg3_id, Some("third"))
        .expect("label");

    session
        .create_branched_session(&msg2_id)
        .expect("create_branched_session");

    assert_eq!(session.get_label(&msg1_id), Some("first"));
    assert_eq!(session.get_label(&msg2_id), Some("second"));
    assert_eq!(session.get_label(&msg3_id), None);
}

#[test]
fn labels_are_not_included_in_build_session_context() {
    let mut session = in_memory();
    let msg_id = session.append_message(user_msg("hello")).expect("append");
    session
        .append_label_change(&msg_id, Some("checkpoint"))
        .expect("label");
    let ctx = session.build_session_context();
    assert_eq!(ctx.messages.len(), 1);
    assert!(matches!(ctx.messages[0], AgentMessage::User(_)));
}

#[test]
fn append_label_change_throws_when_labeling_nonexistent_entry() {
    let mut session = in_memory();
    let err = session
        .append_label_change("non-existent", Some("label"))
        .expect_err("must fail");
    assert_eq!(
        err.to_string(),
        "session error: Entry non-existent not found"
    );
}

// ===========================================================================
// save-entry.test.ts
// ===========================================================================

#[test]
fn saves_custom_entries_and_includes_them_in_tree_traversal() {
    let mut session = in_memory();
    let msg_id = session.append_message(user_msg("hello")).expect("append");
    let custom_id = session
        .append_custom_entry("my_data", Some(json!({"foo": "bar"})))
        .expect("append");
    let msg2_id = session.append_message(assistant_msg("hi")).expect("append");

    let entries = session.get_entries();
    assert_eq!(entries.len(), 3);
    let custom = entries
        .iter()
        .find(|e| e.type_tag() == "custom")
        .expect("custom entry");
    match custom.known() {
        Some(SessionEntry::Custom(c)) => {
            assert_eq!(c.custom_type, "my_data");
            assert_eq!(c.data, Some(json!({"foo": "bar"})));
        }
        other => panic!("expected custom, got {other:?}"),
    }
    assert_eq!(custom.id(), custom_id);
    assert_eq!(custom.parent_id(), Some(msg_id.as_str()));

    let path = session.get_branch(None);
    assert_eq!(entry_ids(&path), [msg_id, custom_id, msg2_id]);

    let ctx = session.build_session_context();
    assert_eq!(ctx.messages.len(), 2, "custom entries skipped in messages");
}

// ===========================================================================
// custom-session-id.test.ts
// ===========================================================================

const UUID_V7_LIKE: fn(&str) -> bool = |id: &str| {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[2].starts_with('7')
        && parts[3].len() == 4
        && matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b'))
        && parts[4].len() == 12
        && parts.iter().all(|p| {
            p.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        })
};

#[test]
fn uses_the_provided_id_instead_of_generating_one() {
    let mut session = in_memory();
    session
        .new_session(NewSessionOptions {
            id: Some("my-custom-id".to_owned()),
            parent_session: None,
        })
        .expect("new_session");
    assert_eq!(session.get_session_id(), "my-custom-id");
}

#[test]
fn uses_the_provided_id_when_creating_an_in_memory_session() {
    let session = SessionManager::in_memory(
        None,
        NewSessionOptions {
            id: Some("memory-session-id".to_owned()),
            parent_session: None,
        },
    )
    .expect("in_memory");
    assert_eq!(session.get_session_id(), "memory-session-id");
    assert_eq!(
        session.get_header().map(|h| h.id.as_str()),
        Some("memory-session-id")
    );
    assert_eq!(session.get_session_file(), None);
}

#[test]
fn allows_alphanumeric_session_ids_with_interior_punctuation() {
    let mut session = in_memory();
    session
        .new_session(NewSessionOptions {
            id: Some("abc-123_def.456".to_owned()),
            parent_session: None,
        })
        .expect("new_session");
    assert_eq!(session.get_session_id(), "abc-123_def.456");
}

#[test]
fn rejects_invalid_custom_session_ids() {
    for id in [
        "", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def",
    ] {
        let mut session = in_memory();
        let err = session
            .new_session(NewSessionOptions {
                id: Some(id.to_owned()),
                parent_session: None,
            })
            .expect_err("must fail");
        assert!(
            err.to_string()
                .contains("Session id must be non-empty, contain only alphanumeric characters"),
            "id {id:?} rejected with wrong message: {err}"
        );
    }
}

#[test]
fn generates_a_uuidv7_id_when_no_id_is_provided() {
    let mut session = in_memory();
    session
        .new_session(NewSessionOptions::default())
        .expect("new_session");
    let id = session.get_session_id();
    assert!(!id.is_empty());
    assert!(UUID_V7_LIKE(id), "not a uuidv7: {id}");
}

#[test]
fn generates_a_uuidv7_id_when_options_provided_without_id() {
    let mut session = in_memory();
    session
        .new_session(NewSessionOptions {
            id: None,
            parent_session: Some("parent.jsonl".to_owned()),
        })
        .expect("new_session");
    assert!(UUID_V7_LIKE(session.get_session_id()));
}

// ===========================================================================
// T07 self-check anchors
// ===========================================================================

/// Deferred persistence: the file is not created before the first assistant message
/// (flushed + wx).
#[test]
fn deferred_persistence_no_file_before_first_assistant() {
    let tmp = TempDir::new();
    let mut session =
        SessionManager::create(tmp.path(), Some(tmp.path()), NewSessionOptions::default())
            .expect("create");
    let file = session.get_session_file().expect("file").to_path_buf();
    assert!(!file.exists(), "no file before first assistant");

    session.append_message(user_msg("hello")).expect("append");
    session
        .append_thinking_level_change("high")
        .expect("append");
    session.append_custom_entry("ext", None).expect("append");
    assert!(!file.exists(), "still no file without assistant");

    session.append_message(assistant_msg("hi")).expect("append");
    assert!(file.exists(), "wx creates file on first assistant");

    // All pre-assistant entries were written in the same flush.
    let content = std::fs::read_to_string(&file).expect("read");
    let lines: Vec<&str> = content.trim().split('\n').collect();
    assert_eq!(lines.len(), 5, "header + 4 entries");

    // Subsequent appends go straight to the file.
    session.append_message(user_msg("more")).expect("append");
    let content = std::fs::read_to_string(&file).expect("read");
    assert_eq!(content.trim().split('\n').count(), 6);
}

/// `wx` exclusive creation: target exists → the error propagates; no overwrite, no panic.
#[test]
fn wx_exclusive_create_fails_when_file_already_exists() {
    let tmp = TempDir::new();
    let mut session =
        SessionManager::create(tmp.path(), Some(tmp.path()), NewSessionOptions::default())
            .expect("create");
    let file = session.get_session_file().expect("file").to_path_buf();
    session.append_message(user_msg("hello")).expect("append");

    // Simulate a stale file at the target path before the first flush.
    std::fs::write(&file, "stale\n").expect("write stale");
    let err = session
        .append_message(assistant_msg("hi"))
        .expect_err("must fail");
    assert!(matches!(err, RpiError::Io(_)), "io error, got: {err}");
    // The stale file is untouched.
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "stale\n");
}

/// Failed append writes propagate the error, no panic (T07 self-check: read-only dir
/// simulation).
#[cfg(unix)]
#[test]
fn append_write_failure_in_readonly_directory_is_error_not_panic() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new();
    let dir = tmp.path().join("readonly");
    std::fs::create_dir_all(&dir).expect("mkdir");

    let mut session = SessionManager::create(tmp.path(), Some(&dir), NewSessionOptions::default())
        .expect("create");
    session.append_message(user_msg("hello")).expect("append");

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    let result = session.append_message(assistant_msg("hi"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod back");

    assert!(
        matches!(result, Err(RpiError::Io(_))),
        "io error, got: {result:?}"
    );
}

/// Unknown entry types: preserved on load → lossless write-back; unknown extension
/// fields on known entries are preserved too (T07 self-check + requirements §6.6).
#[test]
fn unknown_entry_types_roundtrip_losslessly() {
    let tmp = TempDir::new();
    let file = tmp.path().join("unknown.jsonl");
    let unknown_line = "{\"type\":\"quantum_state\",\"id\":\"q1\",\"parentId\":\"m1\",\"timestamp\":\"2025-01-01T00:00:02.000Z\",\"payload\":{\"entangled\":true}}";
    let extra_field_line = "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"q1\",\"timestamp\":\"2025-01-01T00:00:03.000Z\",\"extensionField\":[1,2,3],\"message\":{\"role\":\"user\",\"content\":\"after\",\"timestamp\":2}}";
    let content = format!(
        "{}\n{}\n{}\n{}\n",
        header_line("s1", "/tmp"),
        user_line("m1", "", "before"),
        unknown_line,
        extra_field_line
    );
    std::fs::write(&file, content).expect("write");

    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");

    // Tree navigation includes the unknown entry; its child stays chained.
    let entries = sm.get_entries();
    assert_eq!(entry_ids(&entries), ["m1", "q1", "m2"]);
    assert_eq!(entries[1].type_tag(), "quantum_state");
    assert!(entries[1].known().is_none());
    assert_eq!(entries[2].parent_id(), Some("q1"));

    // No LLM context from unknown entries.
    let ctx = sm.build_session_context();
    assert_eq!(ctx.messages.len(), 2);
    assert_eq!(msg_text(&ctx.messages[0]), "before");
    assert_eq!(msg_text(&ctx.messages[1]), "after");

    // Export preserves the original lines byte-for-byte.
    let exported = sm.export_jsonl().expect("export");
    assert!(
        exported.contains(unknown_line),
        "unknown line preserved:\n{exported}"
    );
    assert!(
        exported.contains(extra_field_line),
        "extension field preserved:\n{exported}"
    );

    // Import primitive: malformed lines skipped, unknown kept; the header
    // line is kept too (upstream `parseSessionEntries` does not filter).
    let parsed = parse_session_entries(&exported);
    assert_eq!(parsed.len(), 4);
    assert!(parsed[0].is_header());
    assert_eq!(parsed[2].type_tag(), "quantum_state");
}

/// Migration assigns id/parentId to unknown types too and keeps the remaining fields
/// (requirements §6.6: write-back does not lose data).
#[test]
fn migration_preserves_unknown_entries_while_assigning_ids() {
    let tmp = TempDir::new();
    let file = tmp.path().join("v1-unknown.jsonl");
    let content = concat!(
        "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"quantum_state\",\"timestamp\":\"2025-01-01T00:00:01.000Z\",\"payload\":42}\n",
        "{\"type\":\"message\",\"timestamp\":\"2025-01-01T00:00:02.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
    );
    std::fs::write(&file, content).expect("write");

    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    let rewritten = std::fs::read_to_string(&file).expect("read");
    let lines: Vec<&str> = rewritten.trim().split('\n').collect();
    let quantum: Value = serde_json::from_str(lines[1]).expect("json");
    assert_eq!(quantum["type"], json!("quantum_state"));
    assert_eq!(quantum["payload"], json!(42), "unknown fields preserved");
    assert!(quantum["id"].is_string(), "migration assigned id");
    assert_eq!(quantum["parentId"], Value::Null);
    let message: Value = serde_json::from_str(lines[2]).expect("json");
    assert_eq!(message["parentId"], quantum["id"]);

    // The unknown entry is the parent of the message in the tree.
    let entries = sm.get_entries();
    assert_eq!(entries[0].type_tag(), "quantum_state");
    assert_eq!(entries[0].parent_id(), None);
}

/// Known entries keep unknown extension fields after the migration rewrite (write-back
/// does not lose data).
#[test]
fn migration_rewrite_preserves_extra_fields_on_known_entries() {
    let tmp = TempDir::new();
    let file = tmp.path().join("v2-extra.jsonl");
    let content = concat!(
        "{\"type\":\"session\",\"version\":2,\"id\":\"s1\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01.000Z\",\"vendor\":{\"x\":1},\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
    );
    std::fs::write(&file, content).expect("write");
    SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    let rewritten = std::fs::read_to_string(&file).expect("read");
    let entry: Value =
        serde_json::from_str(rewritten.trim().split('\n').nth(1).expect("line")).expect("json");
    assert_eq!(entry["vendor"], json!({"x": 1}));
}

/// session_info sanitize: `\r\n` → space (T07 self-check).
#[test]
fn append_session_info_sanitizes_newlines() {
    let mut session = in_memory();
    session.append_message(user_msg("hello")).expect("append");
    session
        .append_session_info("line one\r\nline two\nline three")
        .expect("append");

    let entries = session.get_entries();
    let info = entries
        .iter()
        .find(|e| e.type_tag() == "session_info")
        .expect("info entry");
    match info.known() {
        Some(SessionEntry::SessionInfo(i)) => {
            assert_eq!(i.name.as_deref(), Some("line one line two line three"))
        }
        other => panic!("expected session_info, got {other:?}"),
    }
    assert_eq!(
        session.get_session_name().as_deref(),
        Some("line one line two line three")
    );
}

#[test]
fn get_session_name_uses_latest_entry_and_honors_clears() {
    let mut session = in_memory();
    session.append_message(user_msg("hello")).expect("append");
    assert_eq!(session.get_session_name(), None);
    session.append_session_info("first name").expect("append");
    assert_eq!(session.get_session_name().as_deref(), Some("first name"));
    session.append_session_info("second name").expect("append");
    assert_eq!(session.get_session_name().as_deref(), Some("second name"));
    session.append_session_info("   ").expect("append");
    assert_eq!(
        session.get_session_name(),
        None,
        "empty name clears the title"
    );
}

/// forkFrom: new header + parentSession + full verbatim copy + wx (T07 self-check).
#[test]
fn fork_from_copies_full_history_with_new_header() {
    let tmp = TempDir::new();
    let source_dir = tmp.path().join("source");
    let target_dir = tmp.path().join("target");
    std::fs::create_dir_all(&source_dir).expect("mkdir");
    std::fs::create_dir_all(&target_dir).expect("mkdir");

    let mut source =
        SessionManager::create(&source_dir, Some(&source_dir), NewSessionOptions::default())
            .expect("create");
    source.append_message(user_msg("q1")).expect("append");
    source.append_message(assistant_msg("a1")).expect("append");
    source
        .append_model_change("openai", "gpt-4")
        .expect("append");
    let source_file = source.get_session_file().expect("file").to_path_buf();
    let source_content = std::fs::read_to_string(&source_file).expect("read");

    let forked = SessionManager::fork_from(
        &source_file,
        &target_dir,
        Some(&target_dir),
        ForkOptions::default(),
    )
    .expect("fork_from");

    // New header with parentSession pointing at the source.
    let header = forked.get_header().expect("header");
    assert_eq!(header.parent_session.as_deref(), source_file.to_str());
    assert_eq!(header.cwd, target_dir.to_string_lossy());
    assert_eq!(header.version, Some(3));

    // All non-header entries copied verbatim.
    let forked_file = forked.get_session_file().expect("file").to_path_buf();
    let forked_content = std::fs::read_to_string(&forked_file).expect("read");
    let source_lines: Vec<&str> = source_content.trim().split('\n').skip(1).collect();
    let forked_lines: Vec<&str> = forked_content.trim().split('\n').skip(1).collect();
    assert_eq!(forked_lines, source_lines, "entries copied verbatim");

    // The fork is continuable: leaf and context intact.
    let ctx = forked.build_session_context();
    assert_eq!(ctx.messages.len(), 2);
    assert_eq!(forked.get_entries().len(), 3);
}

#[test]
fn fork_from_rejects_empty_or_invalid_source() {
    let tmp = TempDir::new();
    let file = tmp.path().join("empty.jsonl");
    std::fs::write(&file, "").expect("write");
    let err =
        SessionManager::fork_from(&file, tmp.path(), Some(tmp.path()), ForkOptions::default())
            .expect_err("must fail");
    assert!(err
        .to_string()
        .contains("source session file is empty or invalid"));
}

/// forkFrom with entry_id + position (harness repo-utils.ts getEntriesToFork;
/// T07 self-check: position before|at and user-message validation).
#[test]
fn fork_from_with_entry_position_at_and_before() {
    let tmp = TempDir::new();
    let mut source =
        SessionManager::create(tmp.path(), Some(tmp.path()), NewSessionOptions::default())
            .expect("create");
    let u1 = source.append_message(user_msg("first")).expect("append");
    let a1 = source
        .append_message(assistant_msg("reply"))
        .expect("append");
    source.append_message(user_msg("second")).expect("append");
    let source_file = source.get_session_file().expect("file").to_path_buf();

    // position: at → includes the target entry itself.
    let fork_at = SessionManager::fork_from(
        &source_file,
        tmp.path(),
        Some(tmp.path()),
        ForkOptions {
            id: None,
            entry_id: Some(a1.clone()),
            position: Some(ForkPosition::At),
        },
    )
    .expect("fork at");
    assert_eq!(entry_ids(&fork_at.get_entries()), [u1.clone(), a1.clone()]);

    // position: before (default) on a user message → path stops at its parent.
    let u2 = source
        .get_branch(None)
        .last()
        .expect("leaf")
        .id()
        .to_owned();
    let fork_before = SessionManager::fork_from(
        &source_file,
        tmp.path(),
        Some(tmp.path()),
        ForkOptions {
            id: None,
            entry_id: Some(u2),
            position: None,
        },
    )
    .expect("fork before");
    assert_eq!(entry_ids(&fork_before.get_entries()), [u1, a1.clone()]);

    // position: before on a non-user message → error.
    let err = SessionManager::fork_from(
        &source_file,
        tmp.path(),
        Some(tmp.path()),
        ForkOptions {
            id: None,
            entry_id: Some(a1),
            position: Some(ForkPosition::Before),
        },
    )
    .expect_err("must fail");
    assert!(
        err.to_string().contains("is not a user message"),
        "got: {err}"
    );
}

/// getPathToRootOrCompaction: the retainedTail form stops at the compaction
/// (inclusive); the firstKeptEntryId form walks up to firstKept
/// (harness jsonl-storage.ts:344-370).
#[test]
fn path_to_root_or_compaction_stops_at_compaction_checkpoint() {
    // retainedTail form: stop at the compaction entry itself.
    let tail = vec![user_msg("kept")];
    let entries = vec![
        msg("1", None, user_msg("old")),
        known(SessionEntry::Compaction(CompactionEntry {
            id: "2".to_owned(),
            parent_id: Some("1".to_owned()),
            timestamp: TS.to_owned(),
            summary: "s".to_owned(),
            first_kept_entry_id: None,
            tokens_before: 10,
            retained_tail: Some(tail),
            details: None,
            usage: None,
            from_hook: None,
        })),
        msg("3", Some("2"), user_msg("new")),
    ];
    let path = path_to_root_or_compaction(&entries, Some("3")).expect("path");
    assert_eq!(entry_ids(&path), ["2", "3"]);

    // firstKeptEntryId form: include down to the first-kept entry.
    let entries = vec![
        msg("1", None, user_msg("very old")),
        msg("2", Some("1"), user_msg("kept start")),
        msg("3", Some("2"), assistant_msg("kept too")),
        compaction("4", Some("3"), "s", "2"),
        msg("5", Some("4"), user_msg("new")),
    ];
    let path = path_to_root_or_compaction(&entries, Some("5")).expect("path");
    assert_eq!(entry_ids(&path), ["2", "3", "4", "5"]);
}

/// Malformed lines are skipped (T07 self-check: read robustness) — the rest of the
/// known file stays usable.
#[test]
fn malformed_lines_are_skipped_and_session_stays_usable() {
    let tmp = TempDir::new();
    let file = tmp.path().join("robust.jsonl");
    let content = format!(
        "{}\nGARBAGE\n{{\"broken\":\n{}\n{}\n\"just a string\"\n",
        header_line("s1", "/tmp"),
        user_line("m1", "", "hi"),
        user_line("m2", "m1", "second"),
    );
    std::fs::write(&file, content).expect("write");
    let sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    assert_eq!(entry_ids(&sm.get_entries()), ["m1", "m2"]);
    assert_eq!(sm.get_leaf_id(), Some("m2"));
}

/// --no-session in-memory session: no disk writes.
#[test]
fn in_memory_session_never_touches_disk() {
    let tmp = TempDir::new();
    let mut session = SessionManager::in_memory(Some(tmp.path()), NewSessionOptions::default())
        .expect("in_memory");
    assert!(!session.is_persisted());
    assert_eq!(session.get_session_file(), None);
    session.append_message(user_msg("hello")).expect("append");
    session.append_message(assistant_msg("hi")).expect("append");
    assert_eq!(std::fs::read_dir(tmp.path()).expect("readdir").count(), 0);
}

/// continueRecent: opens the most recent session if any, otherwise creates one.
#[test]
fn continue_recent_opens_most_recent_or_creates_new() {
    let tmp = TempDir::new();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");

    // None yet → new session.
    let fresh = SessionManager::continue_recent(&project, Some(tmp.path())).expect("continue");
    assert!(fresh.get_entries().is_empty());
    let fresh_file = fresh.get_session_file().expect("file").to_path_buf();

    // Persist an assistant message so it materializes.
    let mut fresh = fresh;
    fresh.append_message(user_msg("q")).expect("append");
    fresh.append_message(assistant_msg("a")).expect("append");
    drop(fresh);

    std::thread::sleep(std::time::Duration::from_millis(20));
    let continued = SessionManager::continue_recent(&project, Some(tmp.path())).expect("continue");
    assert_eq!(continued.get_session_file(), Some(fresh_file.as_path()));
    assert_eq!(continued.get_entries().len(), 2);
}

/// Time helpers: toISOString round-trip.
#[test]
fn iso8601_format_parse_roundtrip() {
    assert_eq!(format_iso8601_ms(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        format_iso8601_ms(1_733_184_000_000),
        "2024-12-03T00:00:00.000Z"
    );
    assert_eq!(
        parse_iso8601_ms("2024-12-03T14:00:00.123Z"),
        Some(1_733_234_400_123)
    );
    assert_eq!(
        parse_iso8601_ms(&format_iso8601_ms(1_733_234_400_123)),
        Some(1_733_234_400_123)
    );
    assert_eq!(parse_iso8601_ms("not a date"), None);
    assert_eq!(parse_iso8601_ms("2024-12-03 14:00:00"), None);
}

/// The timestamp `:.→-` rule for file names (session-format.md §File Location).
#[test]
fn session_file_name_replaces_colons_and_dots() {
    let tmp = TempDir::new();
    let session =
        SessionManager::create(tmp.path(), Some(tmp.path()), NewSessionOptions::default())
            .expect("create");
    let file = session.get_session_file().expect("file");
    let name = file.file_name().expect("name").to_string_lossy();
    let (ts, id_part) = name.split_once('_').expect("timestamp_id");
    assert!(
        !ts.contains(':') && !ts.contains('.'),
        "timestamp sanitized: {ts}"
    );
    assert!(id_part.ends_with(".jsonl"));
    assert!(
        id_part.trim_end_matches(".jsonl").contains('-'),
        "uuid part"
    );
}

#[test]
fn parse_iso8601_ms_handles_civil_algorithm_edges() {
    // Leap year Feb 29 and year boundary.
    assert_eq!(
        parse_iso8601_ms("2024-02-29T12:00:00.000Z").map(|ms| format_iso8601_ms(ms as u64)),
        Some("2024-02-29T12:00:00.000Z".to_owned())
    );
    assert_eq!(
        parse_iso8601_ms("2023-03-01T00:00:00.000Z").map(|ms| format_iso8601_ms(ms as u64)),
        Some("2023-03-01T00:00:00.000Z".to_owned())
    );
}

// ===========================================================================
// Review-round regression tests (extension-field preservation, label replay
// order, getBranch unknown id, fork validation-before-write, migration edges)
// ===========================================================================

/// Unknown extension fields on known entry types survive createBranchedSession /
/// forkFrom
/// (session-manager.ts:1426 `{...entry, parentId}`).
#[test]
fn branch_and_fork_preserve_unknown_extension_fields() {
    let tmp = TempDir::new();
    let file = tmp.path().join("ext.jsonl");
    let content = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01.000Z\",\"vendor\":{\"x\":1},\"message\":{\"role\":\"user\",\"content\":\"q\",\"timestamp\":1}}\n",
        "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2025-01-01T00:00:02.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[],\"api\":\"a\",\"provider\":\"p\",\"model\":\"m\",\"usage\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":0,\"cost\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0}},\"stopReason\":\"stop\",\"timestamp\":2}}\n",
    );
    std::fs::write(&file, content).expect("write");

    // createBranchedSession re-chains the path — the vendor field must survive.
    let mut sm = SessionManager::open(&file, Some(tmp.path()), None).expect("open");
    let branched_file = sm
        .create_branched_session("m2")
        .expect("branch")
        .expect("file-backed branch returns a file");
    let branched = std::fs::read_to_string(&branched_file).expect("read branched");
    assert!(
        branched.contains("\"vendor\":{\"x\":1}"),
        "extension field lost in branch:\n{branched}"
    );

    // forkFrom with entry selection — same preservation via the raw view.
    let fork_dir = TempDir::new();
    let forked = SessionManager::fork_from(
        &file,
        tmp.path(),
        Some(fork_dir.path()),
        ForkOptions {
            id: None,
            entry_id: Some("m2".to_owned()),
            position: Some(ForkPosition::At),
        },
    )
    .expect("fork");
    let forked_content =
        std::fs::read_to_string(forked.get_session_file().expect("file")).expect("read fork");
    assert!(
        forked_content.contains("\"vendor\":{\"x\":1}"),
        "extension field lost in fork:\n{forked_content}"
    );
}

/// createBranchedSession replays labels in JS Map insertion order (not HashMap
/// iteration order).
#[test]
fn branched_session_replays_labels_in_insertion_order() {
    let mut session = in_memory();
    let m1 = session.append_message(user_msg("one")).expect("append");
    let m2 = session
        .append_message(assistant_msg("two"))
        .expect("append");
    let m3 = session.append_message(user_msg("three")).expect("append");
    // Set labels in a non-path order: m2 first, then m1.
    session
        .append_label_change(&m2, Some("second"))
        .expect("label");
    session
        .append_label_change(&m1, Some("first"))
        .expect("label");

    session.create_branched_session(&m3).expect("branch");
    let entries = session.get_entries();
    let label_targets: Vec<String> = entries
        .iter()
        .filter(|e| e.type_tag() == "label")
        .filter_map(|e| match e.known() {
            Some(SessionEntry::Label(l)) => Some(l.target_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        label_targets,
        [m2, m1],
        "labels replayed in insertion order"
    );
    // Label entries chain after the last path entry, in the same order.
    let ids = entry_ids(&entries);
    assert_eq!(ids.len(), 5, "3 path entries + 2 label entries: {ids:?}");
    assert_eq!(entries[3].parent_id(), Some(m3.as_str()));
    assert_eq!(entries[4].parent_id(), Some(ids[3]));
}

/// The method-level getBranch returns [] for unknown ids (the fallback only exists in
/// the free buildSessionPath).
#[test]
fn get_branch_with_unknown_id_returns_empty() {
    let mut session = in_memory();
    session.append_message(user_msg("hello")).expect("append");
    assert!(session.get_branch(Some("nonexistent")).is_empty());
    assert_eq!(session.get_branch(None).len(), 1);
}

/// A forkFrom entry-validation failure must not leave an orphan header file on disk
/// (harness jsonl-repo.ts:138-147 validates before creating).
#[test]
fn fork_from_validation_failure_leaves_no_orphan_file() {
    let tmp = TempDir::new();
    let mut source =
        SessionManager::create(tmp.path(), Some(tmp.path()), NewSessionOptions::default())
            .expect("create");
    source.append_message(user_msg("q")).expect("append");
    source.append_message(assistant_msg("a")).expect("append");
    let source_file = source.get_session_file().expect("file").to_path_buf();

    let fork_dir = TempDir::new();
    let files_before = std::fs::read_dir(fork_dir.path()).expect("readdir").count();
    let err = SessionManager::fork_from(
        &source_file,
        tmp.path(),
        Some(fork_dir.path()),
        ForkOptions {
            id: None,
            entry_id: Some("nonexistent".to_owned()),
            position: Some(ForkPosition::At),
        },
    )
    .expect_err("must fail");
    assert!(err.to_string().contains("not found"), "got: {err}");
    let files_after = std::fs::read_dir(fork_dir.path()).expect("readdir").count();
    assert_eq!(files_before, files_after, "no orphan file left behind");
}

/// Migration: firstKeptEntryIndex is removed for any JSON number (including floats)
/// (session-manager.ts:248-254 `typeof === "number"`).
#[test]
fn migrate_v1_removes_float_first_kept_entry_index() {
    let mut entries = vec![
        json!({"type":"session","id":"s1","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp"}),
        json!({"type":"message","timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"hi","timestamp":1}}),
        json!({"type":"compaction","timestamp":"2025-01-01T00:00:02.000Z","summary":"s","firstKeptEntryIndex":1.5}),
    ];
    migrate_session_entries(&mut entries);
    let compaction = &entries[2];
    assert!(
        compaction.get("firstKeptEntryIndex").is_none(),
        "float index key removed: {compaction}"
    );
    // 1.5 cannot resolve to a target (JS array indexing) → no id assigned.
    assert!(compaction.get("firstKeptEntryId").is_none(), "{compaction}");
}

/// Migration: a float 2.0 header version follows JS number semantics — not treated as
/// v1 (no v1→v2 id/parentId migration), but v2→v3 still runs.
#[test]
fn migrate_float_version_2_0_is_not_treated_as_v1() {
    let mut entries = vec![
        json!({"type":"session","version":2.0,"id":"s1","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp"}),
        json!({"type":"message","id":"m1","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"hookMessage","content":"hi","timestamp":1}}),
    ];
    migrate_session_entries(&mut entries);
    // v1→v2 must NOT have run: the entry keeps its original id.
    assert_eq!(entries[1]["id"], json!("m1"));
    // v2→v3 did run: hookMessage → custom, header bumped to 3.
    assert_eq!(entries[1]["message"]["role"], json!("custom"));
    assert_eq!(entries[0]["version"], json!(3));
}
