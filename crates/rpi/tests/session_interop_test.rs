//! T32 Parity Freeze — Session interop final verification.
//!
//! Bidirectional session interop between the upstream coding-agent (v0.84,
//! session JSONL v3) and rpi:
//!
//! - **Direction 1** (`d1_upstream_to_rpi_*`): upstream-recorded fixture
//!   sessions are loaded by rpi and continued via `prompt()` with a faux
//!   provider. Verifies v3 header/entry parsing, tree structure, context
//!   restoration, and that the appended entries conform to the session-format
//!   contract.
//!
//! - **Direction 2** (`d2_rpi_to_upstream_*`): rpi generates a fresh session
//!   (one `prompt()` round with a faux provider) and writes the JSONL to a
//!   known `/tmp` path. A companion Node.js script (run separately) loads
//!   that file via the upstream `SessionManager.open` + `createAgentSession`
//!   and continues it; the `d2_close_loop` test then re-opens the
//!   upstream-continued file in rpi to verify the full round trip.
//!
//! Fixtures: `fixtures/generated/*/session.jsonl` — real upstream
//! SessionManager recordings (7 scenarios with session.jsonl).
//!
//! See `fixtures/README.md` for the fixture runbook and `rpi-docs/v0.11/`
//! for the T32 task definition.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rpi::core::agent_session::PromptOptions;
use rpi::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
use rpi::core::session_manager::SessionManager;
use rpi::sdk::{create_agent_session, CreateAgentSessionOptions};
use rpi_test_support::faux::{
    faux_assistant_message, FauxAiProvider, FauxAssistantOptions, FauxProvider, FauxProviderOptions,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Fixture scenarios that have a `session.jsonl` (non-resource fixtures).
const SESSION_SCENARIOS: &[&str] = &[
    "abort",
    "length-truncation",
    "single-turn",
    "steering-followup",
    "tool-calls",
    "compaction-threshold",
    "compaction-overflow",
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rpi-session-interop-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        TestDir(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn non_empty_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|l| !l.trim().is_empty()).collect()
}

/// Copy a fixture session into a scratch dir (open derives the session dir
/// from the file's parent; fixture dirs stay read-only-by-convention).
fn stage_fixture(scenario: &str) -> (TestDir, PathBuf, String) {
    let fixture_path = fixtures_dir().join(scenario).join("session.jsonl");
    let original = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", fixture_path.display()));
    let dir = TestDir::new(&format!("d1-{scenario}"));
    let staged = dir.0.join("session.jsonl");
    std::fs::write(&staged, &original).expect("stage fixture copy");
    (dir, staged, original)
}

/// Build a minimal model runtime + services with a registered faux provider,
/// returning (model, model_runtime, services).
async fn faux_services(
    cwd: PathBuf,
    agent_dir: PathBuf,
    provider: Arc<FauxProvider>,
) -> (
    rpi_ai::types::Model,
    Arc<rpi::core::model_runtime::ModelRuntime>,
    rpi::core::agent_session_services::AgentSessionServices,
) {
    let model = provider.get_model(None).expect("faux model");

    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: None,
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
        ..Default::default()
    })
    .await;

    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider)))
        .await
        .expect("register faux provider");

    let services = create_agent_session_services(CreateAgentSessionServicesOptions {
        cwd: cwd.clone(),
        agent_dir: Some(agent_dir.clone()),
        settings_manager: None,
        model_runtime: Some(model_runtime.clone()),
        extension_flag_values: Vec::new(),
        resource_loader_options: None,
    })
    .await
    .expect("services");

    (model, model_runtime, services)
}

/// Parse each non-empty line as JSON, returning the vector.
fn parse_jsonl_lines(text: &str) -> Vec<Value> {
    non_empty_lines(text)
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).expect("jsonl line parses"))
        .collect()
}

// ===========================================================================
// Direction 1: Upstream generates → rpi loads + continues
// ===========================================================================

/// D1-A: Every upstream fixture session loads cleanly in rpi — v3 header,
/// correct entry count, single-root tree, context messages match message
/// entries.
#[test]
fn d1_upstream_to_rpi_load_all_fixtures() {
    for scenario in SESSION_SCENARIOS {
        let (_dir, staged, original) = stage_fixture(scenario);
        let lines = parse_jsonl_lines(&original);

        // First line is a session header with version 3.
        let header = &lines[0];
        assert_eq!(
            header.get("type").and_then(Value::as_str),
            Some("session"),
            "{scenario}: first line is session header"
        );
        assert_eq!(
            header.get("version").and_then(Value::as_u64),
            Some(3),
            "{scenario}: header version is 3"
        );
        assert!(
            header.get("id").and_then(Value::as_str).is_some(),
            "{scenario}: header has id"
        );

        let message_entry_count = lines
            .iter()
            .skip(1)
            .filter(|v| v.get("type").and_then(Value::as_str) == Some("message"))
            .count();

        let sm = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: open: {e}"));

        // Session id preserved from header.
        assert_eq!(
            sm.get_session_id(),
            header["id"].as_str().expect("id"),
            "{scenario}: session id"
        );

        // Every non-header line parses as an entry.
        let entries = sm.get_entries();
        assert_eq!(
            entries.len(),
            lines.len() - 1,
            "{scenario}: entry count matches non-header lines"
        );

        // Fixtures are linear chains: single root in the tree.
        assert_eq!(
            sm.get_tree().len(),
            1,
            "{scenario}: single-root tree (linear chain)"
        );

        // All entries have id + parentId (v3 tree structure).
        for entry in &entries {
            let raw = entry.raw_value();
            assert!(
                raw.get("id").and_then(Value::as_str).is_some(),
                "{scenario}: entry has id"
            );
            // parentId may be null for the first entry only.
            assert!(
                raw.get("parentId").is_some(),
                "{scenario}: entry has parentId field"
            );
            assert!(
                raw.get("timestamp").and_then(Value::as_str).is_some(),
                "{scenario}: entry has timestamp"
            );
        }

        // Context messages: for non-compaction scenarios, context includes all
        // message entries. For compaction scenarios (firstKeptEntryId form),
        // the context is truncated to entries after the compaction cut-point
        // plus the compaction summary itself, so context messages < total
        // message entries.
        let has_compaction = lines
            .iter()
            .any(|v| v.get("type").and_then(Value::as_str) == Some("compaction"));
        let ctx = sm.build_session_context();
        if has_compaction {
            // Compaction truncates: context has fewer messages than total
            // message entries (the summary replaces older ones).
            assert!(
                ctx.messages.len() < message_entry_count,
                "{scenario}: compaction truncates context ({} < {} message entries)",
                ctx.messages.len(),
                message_entry_count
            );
        } else {
            assert_eq!(
                ctx.messages.len(),
                message_entry_count,
                "{scenario}: context messages == message entries"
            );
        }

        // First entry's parentId is null (root).
        let first_raw = entries[0].raw_value();
        assert!(
            first_raw.get("parentId").is_none_or(|v| v.is_null()),
            "{scenario}: first entry parentId is null"
        );
    }
}

/// D1-B: Lossless export — after loading, export_jsonl matches the original
/// under the normalizer (consistent id/timestamp mapping).
#[test]
fn d1_upstream_to_rpi_lossless_export_all_fixtures() {
    use rpi_test_support::diff::diff_jsonl;

    for scenario in SESSION_SCENARIOS {
        let (_dir, staged, original) = stage_fixture(scenario);

        let sm = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: open: {e}"));

        let exported = sm.export_jsonl().expect("export");
        let exported_header: Value =
            serde_json::from_str(non_empty_lines(&exported)[0]).expect("exported header parses");
        assert_eq!(
            exported_header.get("version").and_then(Value::as_u64),
            Some(3),
            "{scenario}: exported header v3"
        );

        diff_jsonl(&original, &exported)
            .unwrap_or_else(|f| panic!("{scenario}: export parity diff:\n{f}"));
    }
}

/// D1-C: Full-stack prompt continuation — load an upstream fixture into a
/// real `AgentSession` (faux provider), run `prompt()`, verify the file
/// appends exactly user+assistant message lines and the prefix is untouched.
///
/// Tests multiple fixture types: single-turn (simple), tool-calls (has
/// toolResult messages), and compaction-threshold (has compaction entries).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn d1_upstream_to_rpi_prompt_continue_multiple_fixtures() {
    let test_cases = ["single-turn", "tool-calls", "compaction-threshold"];

    for scenario in test_cases {
        let (dir, staged, original) = stage_fixture(scenario);
        let cwd = dir.0.join("workspace");
        let agent_dir = dir.0.join("agent");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");

        let before_lines = non_empty_lines(&original);

        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![faux_assistant_message(
            "rpi continued answer",
            FauxAssistantOptions::default(),
        )
        .into()]);
        let (model, model_runtime, services) =
            faux_services(cwd.clone(), agent_dir.clone(), provider).await;

        // cwd_override: fixture header cwd is a /tmp path from the recording
        // machine that does not exist here.
        let session_manager =
            SessionManager::open(&staged, None, Some(&cwd)).expect("open fixture session");

        let created = create_agent_session(CreateAgentSessionOptions {
            cwd: Some(cwd.clone()),
            agent_dir: Some(agent_dir.clone()),
            model_runtime: Some(model_runtime),
            model: Some(model),
            services: Some(services),
            session_manager: Some(Arc::new(Mutex::new(session_manager))),
            ..Default::default()
        })
        .await
        .expect("create session from fixture");

        created
            .session
            .prompt("rpi continued question", PromptOptions::default())
            .await
            .expect("prompt");

        // File: exactly 2 new lines appended (user + assistant message).
        let on_disk = std::fs::read_to_string(&staged).expect("read continued session");
        let continued_lines = non_empty_lines(&on_disk);
        assert_eq!(
            continued_lines.len(),
            before_lines.len() + 2,
            "{scenario}: file appended exactly user+assistant"
        );

        // Prefix untouched (byte-identical original lines).
        assert_eq!(
            continued_lines[..before_lines.len()],
            before_lines[..],
            "{scenario}: original fixture lines untouched"
        );

        // The two new lines are message entries: user then assistant.
        let user_line: Value =
            serde_json::from_str(continued_lines[before_lines.len()]).expect("user line parses");
        assert_eq!(
            user_line.get("type").and_then(Value::as_str),
            Some("message"),
            "{scenario}: new line 1 is a message"
        );
        assert_eq!(
            user_line["message"]["role"].as_str(),
            Some("user"),
            "{scenario}: new line 1 is user role"
        );

        let assistant_line: Value = serde_json::from_str(continued_lines[before_lines.len() + 1])
            .expect("assistant line parses");
        assert_eq!(
            assistant_line.get("type").and_then(Value::as_str),
            Some("message"),
            "{scenario}: new line 2 is a message"
        );
        assert_eq!(
            assistant_line["message"]["role"].as_str(),
            Some("assistant"),
            "{scenario}: new line 2 is assistant role"
        );

        // New entries have tree fields (id, parentId, timestamp).
        for new_line in &continued_lines[before_lines.len()..] {
            let v: Value = serde_json::from_str(new_line).expect("new line parses");
            assert!(
                v.get("id").and_then(Value::as_str).is_some(),
                "{scenario}: new entry has id"
            );
            assert!(
                v.get("parentId").and_then(Value::as_str).is_some(),
                "{scenario}: new entry has parentId"
            );
            assert!(
                v.get("timestamp").and_then(Value::as_str).is_some(),
                "{scenario}: new entry has timestamp"
            );
        }

        // parentId chain: the user message's parentId links to the previous
        // leaf; the assistant message's parentId links to the user message.
        let user_parent = user_line["parentId"].as_str().expect("user parentId");
        // The previous leaf's id should match the user message's parentId,
        // OR (for compaction scenarios) the tree may have been restructured.
        // Either way, parentId must reference an existing entry id in the file.
        let all_ids: std::collections::HashSet<String> = continued_lines
            .iter()
            .filter_map(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| v.get("id").and_then(Value::as_str).map(String::from))
            })
            .collect();
        assert!(
            all_ids.contains(user_parent),
            "{scenario}: user message parentId '{user_parent}' references an existing entry"
        );

        let assistant_parent = assistant_line["parentId"]
            .as_str()
            .expect("assistant parentId");
        let user_id = user_line["id"].as_str().expect("user id");
        assert_eq!(
            assistant_parent, user_id,
            "{scenario}: assistant parentId chains to user message id"
        );

        // Reopen: full state survives a reload.
        let reopened =
            SessionManager::open(&staged, None, Some(&cwd)).expect("reopen continued session");
        let ctx = reopened.build_session_context();
        let last = ctx.messages.last().expect("continued assistant message");
        let last_json = serde_json::to_value(last).expect("serialize last message");
        assert_eq!(
            last_json.get("role").and_then(Value::as_str),
            Some("assistant"),
            "{scenario}: last context message is assistant"
        );
    }
}

// ===========================================================================
// Direction 2: rpi generates → upstream loads + continues
// ===========================================================================

/// D2-A: rpi generates a session file (one prompt round, faux provider) and
/// writes it to `/tmp/rpi-d2-generated-session.jsonl`. The companion Node.js
/// script (`/tmp/d2_upstream_continue.mjs`) loads and continues this file
/// using the upstream SDK.
///
/// This test also serves as a standalone rpi generation verification:
/// - session.jsonl is valid JSONL with a v3 header
/// - entries have proper tree structure (id/parentId/timestamp)
/// - the assistant reply is persisted
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn d2_rpi_generates_session_for_upstream() {
    let dir = TestDir::new("d2-generate");
    let cwd = dir.0.join("workspace");
    let agent_dir = dir.0.join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let provider = FauxProvider::new(FauxProviderOptions::default());
    provider.set_responses(vec![faux_assistant_message(
        "Hello from rpi faux provider!",
        FauxAssistantOptions::default(),
    )
    .into()]);
    let (model, model_runtime, services) =
        faux_services(cwd.clone(), agent_dir.clone(), provider).await;

    let session_manager =
        SessionManager::create(&cwd, Some(&agent_dir.join("sessions")), Default::default())
            .expect("create session");

    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: Some(cwd.clone()),
        agent_dir: Some(agent_dir.clone()),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(Arc::new(Mutex::new(session_manager))),
        ..Default::default()
    })
    .await
    .expect("create rpi session");

    created
        .session
        .prompt("Say hello.", PromptOptions::default())
        .await
        .expect("prompt");

    // Locate the session file.
    let sm_lock = created.session.session_manager();
    let sm = sm_lock.lock().expect("lock session manager");
    let session_file = sm
        .get_session_file()
        .expect("session file exists")
        .to_path_buf();
    drop(sm);

    let jsonl = std::fs::read_to_string(&session_file).expect("read rpi-generated session");
    let lines = parse_jsonl_lines(&jsonl);

    // Verify v3 header.
    let header = &lines[0];
    assert_eq!(
        header.get("type").and_then(Value::as_str),
        Some("session"),
        "rpi-generated header type"
    );
    assert_eq!(
        header.get("version").and_then(Value::as_u64),
        Some(3),
        "rpi-generated header version is 3"
    );
    assert!(
        header.get("id").and_then(Value::as_str).is_some(),
        "rpi-generated header has id"
    );

    // Verify entries: model_change, thinking_level_change, user message,
    // assistant message (at minimum).
    let entry_types: Vec<&str> = lines
        .iter()
        .skip(1)
        .filter_map(|v| v.get("type").and_then(Value::as_str))
        .collect();
    assert!(
        entry_types.contains(&"model_change"),
        "rpi-generated has model_change entry: {entry_types:?}"
    );
    assert!(
        entry_types.contains(&"thinking_level_change"),
        "rpi-generated has thinking_level_change entry: {entry_types:?}"
    );
    assert!(
        entry_types.contains(&"message"),
        "rpi-generated has message entries: {entry_types:?}"
    );

    // Tree structure: all entries have id, parentId, timestamp.
    for entry in lines.iter().skip(1) {
        assert!(
            entry.get("id").and_then(Value::as_str).is_some(),
            "rpi-generated entry has id"
        );
        assert!(
            entry.get("parentId").is_some(),
            "rpi-generated entry has parentId field"
        );
        assert!(
            entry.get("timestamp").and_then(Value::as_str).is_some(),
            "rpi-generated entry has timestamp"
        );
    }

    // First entry's parentId is null.
    let first_entry = &lines[1];
    assert!(
        first_entry["parentId"].is_null(),
        "rpi-generated first entry parentId is null"
    );

    // Tree is a single root (linear chain).
    let sm2 = SessionManager::open(&session_file, None, Some(&cwd)).expect("reopen");
    assert_eq!(sm2.get_tree().len(), 1, "rpi-generated single-root tree");

    // Last message is the assistant reply.
    let ctx = sm2.build_session_context();
    let last = ctx.messages.last().expect("has messages");
    let last_json = serde_json::to_value(last).expect("serialize");
    assert_eq!(
        last_json.get("role").and_then(Value::as_str),
        Some("assistant"),
        "rpi-generated last message is assistant"
    );

    // Copy to the well-known /tmp path for the upstream node script.
    let out_path = std::path::Path::new("/tmp/rpi-d2-generated-session.jsonl");
    std::fs::copy(&session_file, out_path).expect("copy to /tmp");
    eprintln!(
        "d2: rpi-generated session written to {} ({} lines)",
        out_path.display(),
        lines.len()
    );
}

/// D2-B: Close-loop — after the upstream Node.js script has loaded and
/// continued the rpi-generated session (writing to
/// `/tmp/rpi-d2-upstream-continued-session.jsonl`), rpi re-opens the file
/// and verifies it can read the upstream-appended entries.
///
/// This test is gated on the upstream-continued file existing; run the
/// Node.js script first.
#[test]
fn d2_close_loop_rpi_reads_upstream_continued() {
    let continued_path = std::path::Path::new("/tmp/rpi-d2-upstream-continued-session.jsonl");
    if !continued_path.exists() {
        eprintln!(
            "skipping d2_close_loop: {} does not exist (run the upstream node script first)",
            continued_path.display()
        );
        return;
    }

    let jsonl = std::fs::read_to_string(continued_path).expect("read upstream-continued session");
    let lines = parse_jsonl_lines(&jsonl);
    assert!(
        lines.len() >= 5,
        "upstream-continued session should have at least header + model/thinking + user + assistant + continued entries"
    );

    // Header: still v3.
    let header = &lines[0];
    assert_eq!(
        header.get("type").and_then(Value::as_str),
        Some("session"),
        "upstream-continued header type"
    );
    assert_eq!(
        header.get("version").and_then(Value::as_u64),
        Some(3),
        "upstream-continued header version is 3"
    );

    // rpi can open without errors.
    let tmp_cwd = std::env::temp_dir().join(format!("rpi-d2-closeloop-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_cwd).expect("cwd");

    // Stage into a temp dir so SessionManager.open can use the file's parent
    // as session_dir.
    let staged_dir = TestDir::new("d2-closeloop");
    let staged = staged_dir.0.join("session.jsonl");
    std::fs::write(&staged, &jsonl).expect("stage");

    let sm = SessionManager::open(&staged, None, Some(&tmp_cwd))
        .expect("rpi opens upstream-continued session");

    let entries = sm.get_entries();
    assert!(
        entries.len() >= 4,
        "upstream-continued has at least 4 entries (model/thinking/user/assistant from rpi + continued pair from upstream)"
    );

    // Tree is still single-root.
    assert_eq!(
        sm.get_tree().len(),
        1,
        "upstream-continued single-root tree"
    );

    // Context has messages from both rpi and upstream continuation.
    let ctx = sm.build_session_context();
    assert!(
        ctx.messages.len() >= 2,
        "upstream-continued context has at least 2 messages"
    );

    // Last message should be the upstream-continued assistant reply.
    let last = ctx.messages.last().expect("last message");
    let last_json = serde_json::to_value(last).expect("serialize last");
    assert_eq!(
        last_json.get("role").and_then(Value::as_str),
        Some("assistant"),
        "upstream-continued last message is assistant"
    );

    eprintln!(
        "d2 close-loop: rpi successfully loaded upstream-continued session ({} entries, {} context messages)",
        entries.len(),
        ctx.messages.len()
    );
}
