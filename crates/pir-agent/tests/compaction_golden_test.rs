//! T08 黄金数值对拍：`fixtures/generated/compaction/golden.json` 与
//! `prompts/*.txt`（由 `fixtures/generate-compaction-golden.mjs` 驱动上游
//! dist 真函数产出）逐值/逐字节比对 `pir_agent::compaction` 的移植实现。
//!
//! 覆盖：estimateTokens / calculateContextTokens / estimateContextTokens /
//! findCutPoint / prepareCompaction / serializeConversation / file ops /
//! prepareBranchEntries / isContextOverflow，以及全部 summarization prompt
//! 渲染（history 初始/更新、turn prefix、split-turn 合并摘要、文件列表追加、
//! branch summary、preamble 拼接、maxTokens/cacheRetention/sessionId 选项）。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pir_agent::compaction::branch_summarization::{
    generate_branch_summary, prepare_branch_entries, GenerateBranchSummaryOptions,
};
use pir_agent::compaction::utils::{
    compute_file_lists, create_file_ops, format_file_operations, serialize_conversation,
    FileOperations, SUMMARIZATION_SYSTEM_PROMPT,
};
use pir_agent::compaction::{
    calculate_context_tokens, compact, estimate_context_tokens, estimate_tokens, find_cut_point,
    generate_summary_with_usage, prepare_compaction, CompactionSettings, SummarizationArgs,
};
use pir_agent::messages::AgentMessage;
use pir_agent::session::SessionEntry;
use pir_agent::stream_fn::BoxStream;
use pir_ai::types::{
    AssistantMessage, Context, DoneReason, Model, StopReason, StreamEvent, StreamOptions, Usage,
};
use serde_json::{json, Value};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated/compaction")
}

fn golden() -> Value {
    let text =
        std::fs::read_to_string(fixtures_dir().join("golden.json")).expect("read golden.json");
    serde_json::from_str(&text).expect("parse golden.json")
}

fn prompt_text(name: &str) -> String {
    std::fs::read_to_string(fixtures_dir().join("prompts").join(name))
        .unwrap_or_else(|e| panic!("read prompts/{name}: {e}"))
}

fn prompt_json(name: &str) -> Value {
    let text = prompt_text(name);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse prompts/{name}: {e}"))
}

fn agent_message(value: &Value) -> AgentMessage {
    serde_json::from_value(value.clone()).expect("AgentMessage deserializes")
}

fn session_entries(value: &Value) -> Vec<SessionEntry> {
    serde_json::from_value(value.clone()).expect("SessionEntry[] deserializes")
}

fn llm_messages(value: &Value) -> Vec<pir_ai::types::Message> {
    serde_json::from_value(value.clone()).expect("Message[] deserializes")
}

fn usage(value: &Value) -> Usage {
    serde_json::from_value(value.clone()).expect("Usage deserializes")
}

fn role_name(message: &AgentMessage) -> &'static str {
    match message {
        AgentMessage::User(_) => "user",
        AgentMessage::Assistant(_) => "assistant",
        AgentMessage::ToolResult(_) => "toolResult",
        AgentMessage::BashExecution(_) => "bashExecution",
        AgentMessage::Custom(_) => "custom",
        AgentMessage::BranchSummary(_) => "branchSummary",
        AgentMessage::CompactionSummary(_) => "compactionSummary",
    }
}

// ---------------------------------------------------------------------------
// Pure-function batteries
// ---------------------------------------------------------------------------

#[test]
fn golden_estimate_tokens() {
    let golden = golden();
    for case in golden["estimateTokens"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        let message = agent_message(&case["message"]);
        assert_eq!(
            estimate_tokens(&message),
            case["expected"].as_u64().expect("expected u64"),
            "estimateTokens/{name}"
        );
    }
}

#[test]
fn golden_calculate_context_tokens() {
    let golden = golden();
    for case in golden["calculateContextTokens"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        assert_eq!(
            calculate_context_tokens(&usage(&case["usage"])),
            case["expected"].as_u64().expect("expected u64"),
            "calculateContextTokens/{name}"
        );
    }
}

#[test]
fn golden_estimate_context_tokens() {
    let golden = golden();
    for case in golden["estimateContextTokens"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        let messages: Vec<AgentMessage> = case["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .map(agent_message)
            .collect();
        let estimate = estimate_context_tokens(&messages);
        let expected = &case["expected"];
        assert_eq!(
            estimate.tokens,
            expected["tokens"].as_u64().expect("tokens"),
            "estimateContextTokens/{name} tokens"
        );
        assert_eq!(
            estimate.usage_tokens,
            expected["usageTokens"].as_u64().expect("usageTokens"),
            "estimateContextTokens/{name} usageTokens"
        );
        assert_eq!(
            estimate.trailing_tokens,
            expected["trailingTokens"].as_u64().expect("trailingTokens"),
            "estimateContextTokens/{name} trailingTokens"
        );
        let expected_index = if expected["lastUsageIndex"].is_null() {
            None
        } else {
            Some(expected["lastUsageIndex"].as_u64().expect("index") as usize)
        };
        assert_eq!(
            estimate.last_usage_index, expected_index,
            "estimateContextTokens/{name} lastUsageIndex"
        );
    }
}

#[test]
fn golden_find_cut_point() {
    let golden = golden();
    for case in golden["findCutPoint"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        let entries = session_entries(&case["entries"]);
        let cut = find_cut_point(
            &entries,
            case["startIndex"].as_u64().expect("startIndex") as usize,
            case["endIndex"].as_u64().expect("endIndex") as usize,
            case["keepRecentTokens"].as_u64().expect("keepRecentTokens"),
        );
        let expected = &case["expected"];
        assert_eq!(
            cut.first_kept_entry_index,
            expected["firstKeptEntryIndex"]
                .as_u64()
                .expect("firstKeptEntryIndex") as usize,
            "findCutPoint/{name} firstKeptEntryIndex"
        );
        // Upstream uses -1 for "no turn start"; Rust uses None.
        let expected_turn_start = match expected["turnStartIndex"].as_i64().expect("turnStartIndex")
        {
            -1 => None,
            value => Some(value as usize),
        };
        assert_eq!(
            cut.turn_start_index, expected_turn_start,
            "findCutPoint/{name} turnStartIndex"
        );
        assert_eq!(
            cut.is_split_turn,
            expected["isSplitTurn"].as_bool().expect("isSplitTurn"),
            "findCutPoint/{name} isSplitTurn"
        );
    }
}

#[test]
fn golden_prepare_compaction() {
    let golden = golden();
    for case in golden["prepareCompaction"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        let entries = session_entries(&case["entries"]);
        let settings: CompactionSettings =
            serde_json::from_value(case["settings"].clone()).expect("settings deserialize");
        let preparation = prepare_compaction(&entries, &settings);
        let expected = &case["expected"];
        if expected.is_null() {
            assert!(
                preparation.is_none(),
                "prepareCompaction/{name}: expected None"
            );
            continue;
        }
        let preparation =
            preparation.unwrap_or_else(|| panic!("prepareCompaction/{name}: missing"));
        assert_eq!(
            preparation.first_kept_entry_id,
            expected["firstKeptEntryId"]
                .as_str()
                .expect("firstKeptEntryId"),
            "prepareCompaction/{name} firstKeptEntryId"
        );
        assert_eq!(
            preparation.is_split_turn,
            expected["isSplitTurn"].as_bool().expect("isSplitTurn"),
            "prepareCompaction/{name} isSplitTurn"
        );
        assert_eq!(
            preparation.tokens_before,
            expected["tokensBefore"].as_u64().expect("tokensBefore"),
            "prepareCompaction/{name} tokensBefore"
        );
        let expected_summary = if expected["previousSummary"].is_null() {
            None
        } else {
            Some(
                expected["previousSummary"]
                    .as_str()
                    .expect("previousSummary"),
            )
        };
        assert_eq!(
            preparation.previous_summary.as_deref(),
            expected_summary,
            "prepareCompaction/{name} previousSummary"
        );
        assert_eq!(
            preparation.messages_to_summarize.len(),
            expected["messagesToSummarizeCount"]
                .as_u64()
                .expect("count") as usize,
            "prepareCompaction/{name} messagesToSummarize"
        );
        assert_eq!(
            preparation.turn_prefix_messages.len(),
            expected["turnPrefixMessagesCount"].as_u64().expect("count") as usize,
            "prepareCompaction/{name} turnPrefixMessages"
        );
        let sorted = |set: &std::collections::HashSet<String>| {
            let mut values: Vec<String> = set.iter().cloned().collect();
            values.sort();
            values
        };
        let expected_ops = &expected["fileOps"];
        for (key, actual) in [
            ("read", sorted(&preparation.file_ops.read)),
            ("written", sorted(&preparation.file_ops.written)),
            ("edited", sorted(&preparation.file_ops.edited)),
        ] {
            let expected_list: Vec<String> = expected_ops[key]
                .as_array()
                .expect("list")
                .iter()
                .map(|v| v.as_str().expect("str").to_owned())
                .collect();
            assert_eq!(
                actual, expected_list,
                "prepareCompaction/{name} fileOps.{key}"
            );
        }
    }
}

#[test]
fn golden_serialize_conversation() {
    let golden = golden();
    for case in golden["serializeConversation"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        let messages = llm_messages(&case["messages"]);
        assert_eq!(
            serialize_conversation(&messages),
            case["expected"].as_str().expect("expected str"),
            "serializeConversation/{name}"
        );
    }
}

#[test]
fn golden_file_ops() {
    let golden = golden();
    for case in golden["fileOps"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        let mut ops: FileOperations = create_file_ops();
        for f in case["ops"]["read"].as_array().expect("read") {
            ops.read.insert(f.as_str().expect("str").to_owned());
        }
        for f in case["ops"]["written"].as_array().expect("written") {
            ops.written.insert(f.as_str().expect("str").to_owned());
        }
        for f in case["ops"]["edited"].as_array().expect("edited") {
            ops.edited.insert(f.as_str().expect("str").to_owned());
        }
        let lists = compute_file_lists(&ops);
        let expected = &case["expected"];
        assert_eq!(
            lists.read_files,
            expected["readFiles"]
                .as_array()
                .expect("readFiles")
                .iter()
                .map(|v| v.as_str().expect("str").to_owned())
                .collect::<Vec<_>>(),
            "fileOps/{name} readFiles"
        );
        assert_eq!(
            lists.modified_files,
            expected["modifiedFiles"]
                .as_array()
                .expect("modifiedFiles")
                .iter()
                .map(|v| v.as_str().expect("str").to_owned())
                .collect::<Vec<_>>(),
            "fileOps/{name} modifiedFiles"
        );
        assert_eq!(
            format_file_operations(&lists.read_files, &lists.modified_files),
            expected["formatted"].as_str().expect("formatted"),
            "fileOps/{name} formatted"
        );
    }
}

#[test]
fn golden_prepare_branch_entries() {
    let golden = golden();
    for case in golden["prepareBranchEntries"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        let entries = session_entries(&case["entries"]);
        let preparation =
            prepare_branch_entries(&entries, case["tokenBudget"].as_u64().expect("tokenBudget"));
        let expected = &case["expected"];
        assert_eq!(
            preparation.messages.len(),
            expected["messageCount"].as_u64().expect("messageCount") as usize,
            "prepareBranchEntries/{name} messageCount"
        );
        assert_eq!(
            preparation.total_tokens,
            expected["totalTokens"].as_u64().expect("totalTokens"),
            "prepareBranchEntries/{name} totalTokens"
        );
        let roles: Vec<&str> = preparation.messages.iter().map(role_name).collect();
        let expected_roles: Vec<&str> = expected["roles"]
            .as_array()
            .expect("roles")
            .iter()
            .map(|v| v.as_str().expect("str"))
            .collect();
        assert_eq!(roles, expected_roles, "prepareBranchEntries/{name} roles");
        let sorted = |set: &std::collections::HashSet<String>| {
            let mut values: Vec<String> = set.iter().cloned().collect();
            values.sort();
            values
        };
        for (key, set) in [
            ("read", sorted(&preparation.file_ops.read)),
            ("written", sorted(&preparation.file_ops.written)),
            ("edited", sorted(&preparation.file_ops.edited)),
        ] {
            let expected_list: Vec<String> = expected["fileOps"][key]
                .as_array()
                .expect("list")
                .iter()
                .map(|v| v.as_str().expect("str").to_owned())
                .collect();
            assert_eq!(
                set, expected_list,
                "prepareBranchEntries/{name} fileOps.{key}"
            );
        }
    }
}

#[test]
fn golden_is_context_overflow() {
    let golden = golden();
    for case in golden["isContextOverflow"].as_array().expect("array") {
        let name = case["name"].as_str().expect("name");
        let message: AssistantMessage =
            serde_json::from_value(case["message"].clone()).expect("AssistantMessage deserializes");
        assert_eq!(
            pir_ai::utils::overflow::is_context_overflow(
                &message,
                Some(case["contextWindow"].as_u64().expect("contextWindow"))
            ),
            case["expected"].as_bool().expect("expected bool"),
            "isContextOverflow/{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Prompt captures (byte-exact renders)
// ---------------------------------------------------------------------------

/// The fixed MODEL of the generator (`captureStreamFn` battery).
fn test_model() -> Model {
    serde_json::from_value(json!({
        "id": "faux-1",
        "name": "Faux",
        "api": "faux",
        "provider": "faux",
        "baseUrl": "http://localhost:0",
        "reasoning": false,
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 128000,
        "maxTokens": 16384,
    }))
    .expect("model deserializes")
}

/// `usageOf(11, 7, 0, 0, 18)` from the generator.
fn capture_usage() -> Usage {
    serde_json::from_value(json!({
        "input": 11,
        "output": 7,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 18,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    }))
    .expect("usage")
}

struct Capture {
    calls: Arc<Mutex<Vec<(Context, StreamOptions)>>>,
    stream_fn: pir_agent::StreamFn,
}

/// `captureStreamFn(texts)` of the generator: each call captures the
/// context/options and completes with the next scripted text.
fn capture_stream_fn(texts: &[&str]) -> Capture {
    let calls: Arc<Mutex<Vec<(Context, StreamOptions)>>> = Arc::new(Mutex::new(Vec::new()));
    let texts: Arc<Vec<String>> = Arc::new(texts.iter().map(|s| (*s).to_owned()).collect());
    let index = Arc::new(Mutex::new(0usize));
    let stream_fn: pir_agent::StreamFn = {
        let calls = calls.clone();
        let texts = texts.clone();
        let index = index.clone();
        Arc::new(
            move |model: Model, context: Context, options: StreamOptions| {
                let i = {
                    let mut index = index.lock().expect("index");
                    let i = (*index).min(texts.len() - 1);
                    *index += 1;
                    i
                };
                calls.lock().expect("calls").push((context, options));
                let message = AssistantMessage {
                    role: pir_ai::types::AssistantRole::Assistant,
                    content: vec![pir_ai::types::AssistantContent::Text(
                        pir_ai::types::TextContent {
                            text: texts[i].clone(),
                            text_signature: None,
                        },
                    )],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    usage: capture_usage(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 99,
                };
                let stream = futures::stream::iter(vec![StreamEvent::Done {
                    reason: DoneReason::Stop,
                    message,
                }]);
                Box::pin(stream) as BoxStream<'static, StreamEvent>
            },
        )
    };
    Capture { calls, stream_fn }
}

fn captured_prompt(capture: &Capture, index: usize) -> String {
    let calls = capture.calls.lock().expect("calls");
    let (context, _) = &calls[index];
    let Some(pir_ai::types::Message::User(user)) = context.messages.first() else {
        panic!("summarization request starts with a user message");
    };
    let pir_ai::types::UserContent::Blocks(blocks) = &user.content else {
        panic!("blocks content");
    };
    let Some(pir_ai::types::UserContentBlock::Text(text)) = blocks.first() else {
        panic!("text block");
    };
    text.text.clone()
}

fn sample_messages() -> Vec<AgentMessage> {
    serde_json::from_value(json!([
        { "role": "user", "content": "Please refactor src/auth.ts to split token handling.", "timestamp": 1 },
        {
            "role": "assistant",
            "content": [
                { "type": "text", "text": "I will read the file first." },
                { "type": "toolCall", "id": "call-1", "name": "read", "arguments": { "path": "src/auth.ts" } },
            ],
            "api": "faux",
            "provider": "faux",
            "model": "faux-1",
            "usage": { "input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15, "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
            "stopReason": "stop",
            "timestamp": 2,
        },
        {
            "role": "toolResult",
            "toolCallId": "call-1",
            "toolName": "read",
            "content": [{ "type": "text", "text": "export function refresh() { /* ... */ }" }],
            "isError": false,
            "timestamp": 3,
        },
    ]))
    .expect("sample messages")
}

#[test]
fn prompt_system_prompt_byte_exact() {
    assert_eq!(
        SUMMARIZATION_SYSTEM_PROMPT,
        prompt_text("system_prompt.txt")
    );
}

#[tokio::test]
async fn prompt_history_initial_byte_exact() {
    let capture = capture_stream_fn(&["SUMMARY ONE"]);
    let args = SummarizationArgs::default();
    generate_summary_with_usage(
        &sample_messages(),
        &test_model(),
        16384,
        None,
        None,
        &capture.stream_fn,
        &args,
        None,
    )
    .await
    .expect("summary");

    assert_eq!(
        captured_prompt(&capture, 0),
        prompt_text("history_initial.txt")
    );
    let options = prompt_json("history_initial_options.json");
    let (_, captured_options) = &capture.calls.lock().expect("calls")[0];
    assert_eq!(
        captured_options.max_tokens,
        Some(options["maxTokens"].as_u64().expect("maxTokens") as u32),
        "maxTokens"
    );
    assert_eq!(
        captured_options.cache_retention,
        Some(pir_ai::types::CacheRetention::None),
        "cacheRetention"
    );
    assert_eq!(options["cacheRetention"], json!("none"));
    assert!(
        captured_options
            .session_id
            .as_deref()
            .is_some_and(|id| !id.is_empty()),
        "sessionId present"
    );
    assert_eq!(options["hasSessionId"], json!(true));
}

#[tokio::test]
async fn prompt_history_update_byte_exact() {
    let capture = capture_stream_fn(&["SUMMARY TWO"]);
    let args = SummarizationArgs::default();
    generate_summary_with_usage(
        &sample_messages(),
        &test_model(),
        16384,
        Some("focus on the token refresh path"),
        Some("## Goal\nPrevious goal text."),
        &capture.stream_fn,
        &args,
        None,
    )
    .await
    .expect("summary");

    assert_eq!(
        captured_prompt(&capture, 0),
        prompt_text("history_update.txt")
    );
}

// ---------------------------------------------------------------------------
// Entry builders (mirror fixtures/generate-compaction-golden.mjs)
// ---------------------------------------------------------------------------

fn big() -> String {
    "x".repeat(400)
}

fn small() -> String {
    "y".repeat(40)
}

/// Builds the exact entry JSON shapes of the generator (`userEntry` /
/// `assistantEntry` / `toolResultEntry` / `compactionEntry` + `chain`).
struct EntryBuilder {
    seq: usize,
    entries: Vec<SessionEntry>,
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn tool_call_block(name: &str, arguments: Value, id: &str) -> Value {
    json!({ "type": "toolCall", "id": id, "name": name, "arguments": arguments })
}

impl EntryBuilder {
    fn new() -> Self {
        Self {
            seq: 0,
            entries: Vec::new(),
        }
    }

    fn next_id(&mut self) -> String {
        self.seq += 1;
        format!("e{:04}", self.seq)
    }

    fn push_json(&mut self, mut value: Value) {
        let parent = self
            .entries
            .last()
            .map(|e| Value::from(e.id().to_owned()))
            .unwrap_or(Value::Null);
        value["parentId"] = parent;
        self.entries
            .push(serde_json::from_value(value).expect("entry deserializes"));
    }

    fn user(&mut self, text: &str) {
        let id = self.next_id();
        self.push_json(json!({
            "type": "message",
            "id": id,
            "timestamp": "2026-08-01T00:00:00.000Z",
            "message": { "role": "user", "content": text, "timestamp": 1 },
        }));
    }

    fn assistant(&mut self, blocks: Vec<Value>) {
        let id = self.next_id();
        self.push_json(json!({
            "type": "message",
            "id": id,
            "timestamp": "2026-08-01T00:00:01.000Z",
            "message": {
                "role": "assistant",
                "content": blocks,
                "api": "faux",
                "provider": "faux",
                "model": "faux-1",
                "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0, "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
                "stopReason": "stop",
                "timestamp": 2,
            },
        }));
    }

    fn tool_result(&mut self, text: &str) {
        let id = self.next_id();
        self.push_json(json!({
            "type": "message",
            "id": id,
            "timestamp": "2026-08-01T00:00:02.000Z",
            "message": {
                "role": "toolResult",
                "toolCallId": "call-1",
                "toolName": "read",
                "content": [{ "type": "text", "text": text }],
                "isError": false,
                "timestamp": 3,
            },
        }));
    }

    fn finish(self) -> Vec<SessionEntry> {
        self.entries
    }
}

fn compare_usage(actual: &Usage, expected: &Value, what: &str) {
    assert_eq!(
        actual.input,
        expected["input"].as_u64().expect("input"),
        "{what} input"
    );
    assert_eq!(
        actual.output,
        expected["output"].as_u64().expect("output"),
        "{what} output"
    );
    assert_eq!(
        actual.total_tokens,
        expected["totalTokens"].as_u64().expect("totalTokens"),
        "{what} totalTokens"
    );
}

/// Split-turn compact() render: history (initial) + turn prefix, merged
/// summary, result payload — prompts §3 of the generator.
#[tokio::test]
async fn prompt_split_turn_compact_byte_exact() {
    let mut b = EntryBuilder::new();
    b.user(&big());
    b.assistant(vec![text_block(&big())]);
    b.user(&big()); // split turn starts here
    b.assistant(vec![text_block(&big())]);
    b.tool_result(&big());
    b.assistant(vec![text_block(&big())]);
    b.tool_result(&small());
    let entries = b.finish();

    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 16384,
        keep_recent_tokens: 150,
    };
    let preparation = prepare_compaction(&entries, &settings).expect("preparation");
    assert!(preparation.is_split_turn, "split-turn preparation");

    let capture = capture_stream_fn(&["HISTORY SUMMARY TEXT", "TURN PREFIX SUMMARY TEXT"]);
    let args = SummarizationArgs::default();
    let result = compact(
        &preparation,
        &test_model(),
        Some("keep the auth details"),
        &capture.stream_fn,
        &args,
        None,
    )
    .await
    .expect("compact");

    assert_eq!(
        captured_prompt(&capture, 0),
        prompt_text("history_in_split_turn.txt")
    );
    assert_eq!(captured_prompt(&capture, 1), prompt_text("turn_prefix.txt"));
    assert_eq!(result.summary, prompt_text("split_turn_merged_summary.txt"));

    let expected = prompt_json("split_turn_result.json");
    assert_eq!(
        result.tokens_before,
        expected["tokensBefore"].as_u64().expect("tokensBefore")
    );
    assert_eq!(
        result.first_kept_entry_id,
        expected["firstKeptEntryId"]
            .as_str()
            .expect("firstKeptEntryId")
    );
    compare_usage(
        result.usage.as_ref().expect("usage"),
        &expected["usage"],
        "split_turn usage",
    );
    assert_eq!(
        result.details.as_ref().expect("details"),
        &expected["details"],
        "split_turn details"
    );
}

/// Non-split compact() with file ops: summary + <modified-files> —
/// prompts §4 of the generator.
#[tokio::test]
async fn prompt_compact_with_file_lists_byte_exact() {
    let mut b = EntryBuilder::new();
    // turn 1 (summarized; carries the file ops)
    b.user(&big());
    b.assistant(vec![
        text_block(&big()),
        tool_call_block("read", json!({ "path": "src/a.ts" }), "call-1"),
        tool_call_block(
            "write",
            json!({ "path": "src/b.ts", "content": "x" }),
            "call-2",
        ),
        tool_call_block(
            "edit",
            json!({ "path": "src/a.ts", "old": "a", "new": "b" }),
            "call-3",
        ),
    ]);
    b.tool_result("body a");
    // turn 2 (kept)
    b.user(&big());
    b.assistant(vec![
        text_block(&small()),
        tool_call_block("bash", json!({ "command": "ls" }), "call-4"),
    ]);
    b.tool_result("ok");
    // turn 3 (kept)
    b.user(&small());
    b.assistant(vec![text_block(&small())]);
    let entries = b.finish();

    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 16384,
        keep_recent_tokens: 120,
    };
    let preparation = prepare_compaction(&entries, &settings).expect("preparation");
    assert!(!preparation.is_split_turn, "non-split preparation");

    let capture = capture_stream_fn(&["HISTORY ONLY SUMMARY"]);
    let args = SummarizationArgs::default();
    let result = compact(
        &preparation,
        &test_model(),
        None,
        &capture.stream_fn,
        &args,
        None,
    )
    .await
    .expect("compact");

    assert_eq!(
        result.summary,
        prompt_text("compact_summary_with_file_lists.txt")
    );
    let expected = prompt_json("compact_result.json");
    assert_eq!(
        result.tokens_before,
        expected["tokensBefore"].as_u64().expect("tokensBefore")
    );
    assert_eq!(
        result.first_kept_entry_id,
        expected["firstKeptEntryId"]
            .as_str()
            .expect("firstKeptEntryId")
    );
    compare_usage(
        result.usage.as_ref().expect("usage"),
        &expected["usage"],
        "compact usage",
    );
    assert_eq!(
        result.details.as_ref().expect("details"),
        &expected["details"],
        "compact details"
    );
    assert_eq!(
        captured_prompt(&capture, 0),
        expected["promptText"].as_str().expect("promptText")
    );
}

/// Branch summary render + preamble/file-list splice — prompts §5/§6.
#[tokio::test]
async fn prompt_branch_summary_byte_exact() {
    let mut b = EntryBuilder::new();
    b.user("Explore the OAuth device flow.");
    b.assistant(vec![
        text_block("Reading the OAuth module."),
        tool_call_block("read", json!({ "path": "src/oauth.ts" }), "call-1"),
    ]);
    b.tool_result("export async function deviceFlow() {}");
    b.assistant(vec![text_block("The device flow polls every 5 seconds.")]);
    let entries = b.finish();

    let capture = capture_stream_fn(&["BRANCH SUMMARY TEXT"]);
    let args = SummarizationArgs::default();
    let options = GenerateBranchSummaryOptions {
        model: &test_model(),
        stream_fn: &capture.stream_fn,
        args: &args,
        custom_instructions: None,
        replace_instructions: false,
        reserve_tokens: 16384,
        callbacks: None,
    };
    let result = generate_branch_summary(&entries, &options).await;

    assert_eq!(captured_prompt(&capture, 0), prompt_text("branch.txt"));
    assert_eq!(
        result.summary.as_deref().expect("summary"),
        prompt_text("branch_result_summary.txt")
    );
    let expected = prompt_json("branch_result.json");
    assert_eq!(
        result.read_files.as_ref().expect("readFiles"),
        &expected["readFiles"]
            .as_array()
            .expect("readFiles")
            .iter()
            .map(|v| v.as_str().expect("str").to_owned())
            .collect::<Vec<_>>()
    );
    assert!(result
        .modified_files
        .as_ref()
        .expect("modifiedFiles")
        .is_empty());
    let (_, captured_options) = &capture.calls.lock().expect("calls")[0];
    assert_eq!(
        captured_options.max_tokens,
        Some(expected["maxTokens"].as_u64().expect("maxTokens") as u32)
    );
    assert_eq!(
        captured_options.cache_retention,
        Some(pir_ai::types::CacheRetention::None)
    );
    compare_usage(
        result.usage.as_ref().expect("usage"),
        &expected["usage"],
        "branch usage",
    );
}

#[tokio::test]
async fn prompt_branch_custom_instructions_byte_exact() {
    let mut b = EntryBuilder::new();
    b.user("short branch");
    b.assistant(vec![text_block("work")]);
    let entries = b.finish();

    let capture = capture_stream_fn(&["BRANCH CUSTOM"]);
    let args = SummarizationArgs::default();
    let options = GenerateBranchSummaryOptions {
        model: &test_model(),
        stream_fn: &capture.stream_fn,
        args: &args,
        custom_instructions: Some("focus on OAuth"),
        replace_instructions: false,
        reserve_tokens: 16384,
        callbacks: None,
    };
    generate_branch_summary(&entries, &options).await;

    assert_eq!(
        captured_prompt(&capture, 0),
        prompt_text("branch_custom_instructions.txt")
    );
}
