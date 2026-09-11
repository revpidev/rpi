//! V14-25 (ADR-0027): `ctx.sessionEntries` — the session-bound read path.
//!
//! `SessionContextActions::get_session_entries` binds
//! `SessionManager::get_branch` (root→leaf, the same in-memory structure for
//! file-backed and `--no-session` hosts) and applies the ADR-0027 filter
//! chain: `type:"custom"` → exact `customType` → tail `limit`. These tests
//! cover the assertion matrix A1–A5/A8 of the task file:
//!
//! - A1 file-backed session: `customType` filter + root→leaf order
//! - A2 in-memory session (`--no-session`): same answers (TE-D41 root cause)
//! - A3 branch navigation: the ACTIVE branch is read, not the file tail
//! - A4 `limit`: filtered tail N, order kept
//! - A5 no active leaf / dropped session → `[]`, no panic (the unbound-host
//!   `[]` is asserted at the dispatch level in `rpi-ext-host`)
//! - A8 only `type:"custom"`: message/custom_message/label entries never leak
//!
//! The dispatch-level contract (arg parsing, host limit cap, capability
//! gate) is covered in `rpi-ext-host` (`host_call.rs` tests +
//! `tests/session_entries_parity.rs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi_ext_host::api::ContextActions;
use rpi_test_support::faux::{
    FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
};

// ---------------------------------------------------------------------------
// Fixture (leaner agent_session_test.rs shape: no scripted responses)
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-session-entries-test-{}-{nanos}-{id}",
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

fn ids(entries: &[rpi_ext_host::types::SessionEntryInfo]) -> Vec<&str> {
    entries.iter().map(|e| e.id.as_str()).collect()
}

/// Append a custom entry, returning its id.
fn append_custom(
    manager: &Mutex<SessionManager>,
    custom_type: &str,
    data: serde_json::Value,
) -> String {
    manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_custom_entry(custom_type, Some(data))
        .expect("append custom entry")
}

// ---------------------------------------------------------------------------
// A1 + A8: file-backed session — filter, order, custom-only projection
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a1_a8_file_session_filters_orders_and_leaks_no_conversation_entries() {
    let fixture = file_session_fixture().await;
    let manager = &fixture.manager;

    // Conversation-adjacent entries (A8): none of these may leak through
    // ctx.sessionEntries — message, custom_message, label.
    let custom_message_id = {
        let mut guard = manager.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .append_message(rpi_agent::messages::AgentMessage::User(
                rpi_ai::types::UserMessage {
                    role: rpi_ai::types::UserRole::User,
                    content: rpi_ai::types::UserContent::Text(
                        "secret conversation text".to_owned(),
                    ),
                    timestamp: 0,
                },
            ))
            .expect("append message");
        let id = guard
            .append_custom_message_entry(
                "custom-msg",
                rpi_ai::types::UserContent::Text("note".to_owned()),
                true,
                None,
            )
            .expect("append custom_message");
        let label_id = guard
            .append_label_change(&id, Some("label"))
            .expect("append label");
        (id, label_id)
    };

    // Custom entries in path order: e1 (matching), e2 (other type), e3
    // (matching, nested data), e4 (matching, no data field).
    let e1 = append_custom(manager, "mcp-approval-v1", serde_json::json!({"n": 1}));
    let e2 = append_custom(manager, "other-type", serde_json::json!({"n": 2}));
    let e3 = append_custom(
        manager,
        "mcp-approval-v1",
        serde_json::json!({"nested": {"k": "v"}}),
    );
    let e4 = append_custom(manager, "mcp-approval-v1", serde_json::Value::Null);

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);

    // A1: customType filter, root→leaf order.
    let filtered = actions.get_session_entries(Some("mcp-approval-v1"), None);
    assert_eq!(
        ids(&filtered),
        vec![e1.as_str(), e3.as_str(), e4.as_str()],
        "exact-match filter keeps path order"
    );

    // A8: without a filter, ALL custom entries — and only custom entries —
    // come back; message/custom_message/label ids never appear.
    let all = actions.get_session_entries(None, None);
    assert_eq!(
        ids(&all),
        vec![e1.as_str(), e2.as_str(), e3.as_str(), e4.as_str()]
    );
    let serialized = serde_json::to_string(&all).expect("serialize");
    assert!(
        !serialized.contains("secret conversation text"),
        "conversation content must not leak"
    );
    assert!(
        !serialized.contains(&custom_message_id.0),
        "custom_message entry must not leak"
    );

    // Wire shape: camelCase, parentId chained root→leaf (the first custom
    // entry chains at its in-path predecessor — the label entry), data
    // verbatim (null for the data-less entry).
    let first = &all[0];
    assert_eq!(
        first.parent_id.as_deref(),
        Some(custom_message_id.1.as_str()),
        "custom entries chain at their in-path predecessor"
    );
    assert_eq!(first.custom_type, "mcp-approval-v1");
    assert_eq!(first.data, serde_json::json!({"n": 1}));
    assert!(!first.timestamp.is_empty(), "timestamp passes through");
    let third = &all[2];
    assert_eq!(
        third.data,
        serde_json::json!({"nested": {"k": "v"}}),
        "verbatim data"
    );
    let fourth = &all[3];
    assert_eq!(fourth.data, serde_json::Value::Null, "no data field → null");
    assert_eq!(fourth.parent_id.as_deref(), Some(e3.as_str()));
    let keys: Vec<_> = serde_json::to_value(fourth)
        .expect("serialize")
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        vec!["id", "parentId", "timestamp", "customType", "data"],
        "frozen ADR-0027 shape"
    );
}

// ---------------------------------------------------------------------------
// A2: in-memory session (TE-D41 root cause) — same answers as file-backed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2_in_memory_session_reads_the_active_branch() {
    let fixture = in_memory_session_fixture().await;
    // No JSONL exists at all for this session (ADR-0022: path: null).
    assert_eq!(fixture.session.session_file(), None);

    let manager = &fixture.manager;
    let e1 = append_custom(manager, "mcp-approval-v1", serde_json::json!({"grant": 1}));
    let _noise = append_custom(manager, "other-type", serde_json::json!({"n": 2}));
    let e3 = append_custom(manager, "mcp-approval-v1", serde_json::json!({"grant": 2}));

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);
    let entries = actions.get_session_entries(Some("mcp-approval-v1"), None);
    assert_eq!(
        ids(&entries),
        vec![e1.as_str(), e3.as_str()],
        "in-memory sessions read the same in-memory branch structure"
    );
}

// ---------------------------------------------------------------------------
// A3: branch navigation — active branch wins over the file tail
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a3_branch_navigation_reads_the_target_branch_not_the_file_tail() {
    let fixture = file_session_fixture().await;
    let manager = &fixture.manager;
    let e1 = append_custom(
        manager,
        "mcp-approval-v1",
        serde_json::json!({"branch": "root"}),
    );
    let _old_tail = append_custom(
        manager,
        "mcp-approval-v1",
        serde_json::json!({"branch": "old"}),
    );

    // Navigate back to e1 (upstream `branch()` — the /tree move): the
    // entries after e1 leave the active branch, even though they are the
    // file tail a JSONL reader would replay.
    manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .branch(&e1)
        .expect("branch to e1");

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);
    assert_eq!(
        ids(&actions.get_session_entries(Some("mcp-approval-v1"), None)),
        vec![e1.as_str()],
        "active branch only — no file-tail heuristic"
    );

    // A new entry on the target branch extends it (parentId = e1).
    let e3 = append_custom(
        manager,
        "mcp-approval-v1",
        serde_json::json!({"branch": "new"}),
    );
    let entries = actions.get_session_entries(Some("mcp-approval-v1"), None);
    assert_eq!(ids(&entries), vec![e1.as_str(), e3.as_str()]);
    assert_eq!(entries[1].parent_id.as_deref(), Some(e1.as_str()));
}

// ---------------------------------------------------------------------------
// A4: limit — filtered tail N, order kept
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a4_limit_keeps_the_filtered_tail_in_path_order() {
    let fixture = in_memory_session_fixture().await;
    let manager = &fixture.manager;
    let wanted: Vec<String> = (0..4)
        .map(|n| append_custom(manager, "mcp-approval-v1", serde_json::json!({"n": n})))
        .collect();
    let noise = append_custom(manager, "other-type", serde_json::json!({"n": 99}));

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);

    // Tail 2 of the 4 matching entries (noise filtered first).
    assert_eq!(
        ids(&actions.get_session_entries(Some("mcp-approval-v1"), Some(2))),
        vec![wanted[2].as_str(), wanted[3].as_str()],
        "limit applies AFTER the customType filter, keeping path order"
    );
    // Limit larger than the filtered set keeps everything.
    assert_eq!(
        ids(&actions.get_session_entries(Some("mcp-approval-v1"), Some(99))),
        ids(&actions.get_session_entries(Some("mcp-approval-v1"), None))
    );
    // No customType filter, only a limit: applies to the full custom set.
    assert_eq!(
        ids(&actions.get_session_entries(None, Some(1))),
        vec![noise.as_str()],
        "tail of the unfiltered custom set"
    );
}

// ---------------------------------------------------------------------------
// A5: fail-closed — no active leaf (reset_leaf) → `[]`, no panic. The
// dropped-session weak-upgrade path and the unbound-host dispatch default
// are the same fail-closed contract ([]).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a5_no_active_leaf_answers_empty_array() {
    let fixture = in_memory_session_fixture().await;
    append_custom(
        &fixture.manager,
        "mcp-approval-v1",
        serde_json::json!({"n": 1}),
    );
    // resetLeaf: leaf_id = None → the active branch is empty.
    fixture
        .manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reset_leaf();

    let actions = rpi::core::extension_context::SessionContextActions::new(&fixture.session);
    let entries = actions.get_session_entries(Some("mcp-approval-v1"), None);
    assert!(entries.is_empty(), "no active leaf → [], got {entries:?}");
    // And the full-host path through the dispatch answers the envelope
    // `[]` for the same state (not an error).
    let unfiltered = actions.get_session_entries(None, None);
    assert!(unfiltered.is_empty());
}
