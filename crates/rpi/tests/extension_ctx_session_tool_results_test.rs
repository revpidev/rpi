//! V15-14 (ADR-0030): `ctx.sessionToolResults` — the session-bound read
//! path.
//!
//! `SessionContextActions::get_session_tool_results` binds
//! `SessionManager::get_branch` (root→leaf, the same in-memory structure
//! for file-backed and `--no-session` hosts) and applies the ADR-0030
//! filter chain: `type:"message"` → `role:"toolResult"` → exact
//! `toolName` → tail `limit`. These tests cover the task-file assertion
//! matrix:
//!
//! - FR-A file-backed session: exact `toolName` filter (same-prefix tools
//!   and other tools never match) + root→leaf order
//! - FR-B projection face: exactly the six ADR-0030 fields; `content`
//!   text blocks never leak; non-matching tools' entries never returned
//! - FR-A limit: filtered tail N, order kept
//! - in-memory session (`--no-session`): same answers (replay channel is
//!   host-memory, the ADR-0030 rationale)
//! - branch navigation: the ACTIVE branch is read, not the file tail
//! - fail-closed: no active leaf → `[]`, no panic (the unbound-host `[]`
//!   is asserted at the dispatch level in `rpi-ext-host`)
//!
//! The dispatch-level contract (required `toolName`, host limit cap,
//! capability gate) is covered in `rpi-ext-host` (`host_call.rs` tests +
//! `tests/session_tool_results_parity.rs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi_agent::messages::AgentMessage;
use rpi_ai::types::{TextContent, ToolResultContent, ToolResultMessage, ToolResultRole};
use rpi_ext_host::api::ContextActions;
use rpi_test_support::faux::{
    FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
};

// ---------------------------------------------------------------------------
// Fixture (same lean shape as extension_ctx_session_entries_test.rs)
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-session-tool-results-test-{}-{nanos}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    session: rpi::core::agent_session::AgentSession,
    manager: Arc<Mutex<SessionManager>>,
    _tmp: TempDir,
}

async fn session_fixture(tmp: TempDir, manager: Arc<Mutex<SessionManager>>) -> Fixture {
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    for dir in [&cwd, &agent_dir] {
        std::fs::create_dir_all(dir).expect("dir");
    }
    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
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
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(manager.clone()),
        ..Default::default()
    })
    .await
    .expect("create session");
    Fixture {
        session: created.session,
        manager,
        _tmp: tmp,
    }
}

/// A file-backed session (session dir under the temp root).
async fn file_session_fixture() -> Fixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let session_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let manager = SessionManager::create(&cwd, Some(&session_dir), NewSessionOptions::default())
        .expect("file-backed session");
    session_fixture(tmp, Arc::new(Mutex::new(manager))).await
}

/// An in-memory session (`--no-session` shape, ADR-0022: no file at all).
async fn in_memory_session_fixture() -> Fixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let manager =
        SessionManager::in_memory(Some(&cwd), NewSessionOptions::default()).expect("in-memory");
    session_fixture(tmp, Arc::new(Mutex::new(manager))).await
}

fn ids(results: &[rpi_ext_host::types::SessionToolResultInfo]) -> Vec<&str> {
    results.iter().map(|r| r.id.as_str()).collect()
}

/// Append a toolResult message entry, returning its id. `content` carries
/// a marker text that must never appear in any projection (FR-B).
fn append_tool_result(
    manager: &Mutex<SessionManager>,
    tool_name: &str,
    details: Option<serde_json::Value>,
    is_error: bool,
) -> String {
    manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_message(AgentMessage::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: format!("call_{tool_name}"),
            tool_name: tool_name.to_owned(),
            content: vec![ToolResultContent::Text(TextContent {
                text: format!("secret content text of {tool_name}"),
                text_signature: None,
            })],
            details,
            usage: None,
            is_error,
            timestamp: 0,
        }))
        .expect("append toolResult message")
}

/// Append a plain user message (conversation content — must never match).
fn append_user_message(manager: &Mutex<SessionManager>, text: &str) -> String {
    manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_message(AgentMessage::User(rpi_ai::types::UserMessage {
            role: rpi_ai::types::UserRole::User,
            content: rpi_ai::types::UserContent::Text(text.to_owned()),
            timestamp: 0,
        }))
        .expect("append user message")
}

// ---------------------------------------------------------------------------
// FR-A + FR-B: file-backed session — exact filter, order, six-field
// projection, content never leaks
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fr_a_fr_b_file_session_exact_filter_order_and_frozen_projection() {
    let fixture = file_session_fixture().await;
    let manager = &fixture.manager;

    // Conversation entries: user message with the same marker text and a
    // same-PREFIX tool ("todo_bulk") plus an unrelated tool — none of them
    // may match `toolName: "todo"` (exact match, FR-A; prefix ≠ match).
    let _user = append_user_message(manager, "secret content text of todo");
    let _prefix = append_tool_result(manager, "todo_bulk", None, false);
    let _other = append_tool_result(manager, "web_search", None, false);

    // Matching results in path order: r1 (ok, envelope), r2 (error, no
    // details), r3 (ok, nested envelope).
    let r1 = append_tool_result(
        manager,
        "todo",
        Some(serde_json::json!({"tasks": [{"id": 1}], "nextId": 2})),
        false,
    );
    let r2 = append_tool_result(manager, "todo", None, true);
    let r3 = append_tool_result(
        manager,
        "todo",
        Some(serde_json::json!({"tasks": [], "nextId": 9, "深": [1, 2]})),
        false,
    );

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);

    // FR-A: exact toolName filter, root→leaf order.
    let results = actions.get_session_tool_results("todo", None);
    assert_eq!(
        ids(&results),
        vec![r1.as_str(), r2.as_str(), r3.as_str()],
        "exact-match filter keeps path order; prefix/other tools never match"
    );

    // FR-A wire values: parentId chains root→leaf at the in-path
    // predecessor (the last non-matching entry), details verbatim.
    assert_eq!(results[0].parent_id.as_deref(), Some(_other.as_str()));
    assert_eq!(results[1].parent_id.as_deref(), Some(r1.as_str()));
    assert_eq!(
        results[0].details,
        serde_json::json!({"tasks": [{"id": 1}], "nextId": 2})
    );
    assert_eq!(results[1].details, serde_json::Value::Null, "absent → null");
    assert!(results[1].is_error);
    assert!(!results[0].timestamp.is_empty(), "timestamp passes through");

    // FR-B red line: the serialized projection is EXACTLY the six
    // ADR-0030 fields — no content blocks, no other message fields.
    let serialized = serde_json::to_string(&results).expect("serialize");
    assert!(
        !serialized.contains("secret content text"),
        "content text blocks must not leak"
    );
    let keys: Vec<_> = serde_json::to_value(&results[0])
        .expect("serialize")
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "parentId",
            "timestamp",
            "toolName",
            "isError",
            "details"
        ],
        "frozen ADR-0030 six-field shape"
    );
}

// ---------------------------------------------------------------------------
// FR-A limit: filtered tail N, order kept
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fr_a_limit_keeps_the_filtered_tail_in_path_order() {
    let fixture = in_memory_session_fixture().await;
    let manager = &fixture.manager;
    let wanted: Vec<String> = (0..4)
        .map(|n| append_tool_result(manager, "todo", Some(serde_json::json!({"n": n})), false))
        .collect();
    // Noise before AND after the matching run — filtered out first.
    let _before = append_tool_result(manager, "web_search", None, false);
    let _after = append_tool_result(manager, "todo_bulk", None, false);

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);

    // Tail 2 of the 4 matching results (noise filtered first).
    assert_eq!(
        ids(&actions.get_session_tool_results("todo", Some(2))),
        vec![wanted[2].as_str(), wanted[3].as_str()],
        "limit applies AFTER the toolName filter, keeping path order"
    );
    // Limit larger than the filtered set keeps everything.
    assert_eq!(
        ids(&actions.get_session_tool_results("todo", Some(99))),
        ids(&actions.get_session_tool_results("todo", None))
    );
}

// ---------------------------------------------------------------------------
// In-memory session — the replay channel is host memory (ADR-0030
// rationale: no JSONL to read, same answers)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_memory_session_reads_the_active_branch() {
    let fixture = in_memory_session_fixture().await;
    // No JSONL exists at all for this session (ADR-0022: path: null).
    assert_eq!(fixture.session.session_file(), None);

    let manager = &fixture.manager;
    let r1 = append_tool_result(
        manager,
        "todo",
        Some(serde_json::json!({"tasks": [], "nextId": 1})),
        false,
    );
    let _noise = append_tool_result(manager, "other_tool", None, false);

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);
    let results = actions.get_session_tool_results("todo", None);
    assert_eq!(
        ids(&results),
        vec![r1.as_str()],
        "in-memory sessions read the same in-memory branch structure"
    );
}

// ---------------------------------------------------------------------------
// Branch navigation — active branch wins over the file tail (replay must
// follow the /tree move, like upstream getBranch())
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branch_navigation_reads_the_target_branch_not_the_file_tail() {
    let fixture = file_session_fixture().await;
    let manager = &fixture.manager;
    let r1 = append_tool_result(
        manager,
        "todo",
        Some(serde_json::json!({"branch": "root"})),
        false,
    );
    let _old_tail = append_tool_result(
        manager,
        "todo",
        Some(serde_json::json!({"branch": "old"})),
        false,
    );

    // Navigate back to r1 (upstream `branch()` — the /tree move): the
    // results after r1 leave the active branch, even though they are the
    // file tail a JSONL reader would replay.
    manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .branch(&r1)
        .expect("branch to r1");

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);
    assert_eq!(
        ids(&actions.get_session_tool_results("todo", None)),
        vec![r1.as_str()],
        "active branch only — no file-tail heuristic"
    );

    // A new result on the target branch extends it (parentId = r1) —
    // the last-write-wins replay input is the branch, in order.
    let r3 = append_tool_result(
        manager,
        "todo",
        Some(serde_json::json!({"branch": "new"})),
        false,
    );
    let results = actions.get_session_tool_results("todo", None);
    assert_eq!(ids(&results), vec![r1.as_str(), r3.as_str()]);
    assert_eq!(results[1].parent_id.as_deref(), Some(r1.as_str()));
}

// ---------------------------------------------------------------------------
// Fail-closed — no active leaf (reset_leaf) → `[]`, no panic. The
// dropped-session weak-upgrade path and the unbound-host dispatch default
// are the same fail-closed contract ([]).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fail_closed_no_active_leaf_answers_empty_array() {
    let fixture = in_memory_session_fixture().await;
    append_tool_result(
        &fixture.manager,
        "todo",
        Some(serde_json::json!({"n": 1})),
        false,
    );
    // resetLeaf: leaf_id = None → the active branch is empty.
    fixture
        .manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reset_leaf();

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);
    let results = actions.get_session_tool_results("todo", None);
    assert!(results.is_empty(), "no active leaf → [], got {results:?}");
}
