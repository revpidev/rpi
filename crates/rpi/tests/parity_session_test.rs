//! G3 parity (T07): `fixtures/generated/*/session.jsonl` (real session
//! recordings from the upstream coding-agent) loaded by
//! `rpi::core::session_manager::SessionManager` → tree/context building →
//! written back via `export_jsonl()`, normalized-diffed against the original
//! fixture.
//!
//! Two levels:
//! - **lossless round trip**: after opening a v3 fixture, the exported line
//!   sequence matches the original under the `rpi_test_support` Normalizer
//!   semantics (consistent id/timestamp mapping).
//! - **load + continue**: after opening a fixture, appended user/assistant
//!   messages put the context into a state that includes old and new
//!   messages, two appended file lines, and a complete state after
//!   reopening.
//!
//! The fixtures contain no compaction/branch_summary/label entries; those
//! shapes are covered by the synthetic unit tests in
//! `core::session_manager::tests`.

use std::path::PathBuf;

use rpi::core::session_manager::SessionManager;
use rpi_agent::messages::AgentMessage;
use rpi_ai::types::{UserContent, UserMessage, UserRole};
use rpi_test_support::diff::diff_jsonl;
use rpi_test_support::faux::{faux_assistant_message, FauxAssistantOptions};
use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "abort",
    "length-truncation",
    "single-turn",
    "steering-followup",
    "tool-calls",
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rpi-parity-session-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create parity session test dir");
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

/// Copy the fixture session into a scratch dir (open derives the session dir
/// from the file's parent; the fixture dir itself stays read-only-by-convention).
fn stage_fixture(scenario: &str) -> (TestDir, PathBuf, String) {
    let fixture_path = fixtures_dir().join(scenario).join("session.jsonl");
    let original = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", fixture_path.display()));
    let dir = TestDir::new(scenario);
    let staged = dir.0.join("session.jsonl");
    std::fs::write(&staged, &original).expect("stage fixture copy");
    (dir, staged, original)
}

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Text(text.to_owned()),
        timestamp: 1,
    })
}

#[test]
fn parity_fixture_sessions_load_and_export_lossless() {
    for scenario in SCENARIOS {
        let (_dir, staged, original) = stage_fixture(scenario);
        let lines = non_empty_lines(&original);
        let header: Value = serde_json::from_str(lines[0]).expect("fixture header parses");
        assert_eq!(
            header.get("type").and_then(Value::as_str),
            Some("session"),
            "{scenario}: first line is a session header"
        );
        let message_lines = lines
            .iter()
            .filter(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
                    .as_deref()
                    == Some("message")
            })
            .count();

        let sm = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: open: {e}"));

        // Header: v3, id preserved.
        assert_eq!(
            sm.get_session_id(),
            header["id"].as_str().expect("id"),
            "{scenario}"
        );

        // Every non-header line is an entry; tree is a single root (fixtures
        // are linear chains).
        let entries = sm.get_entries();
        assert_eq!(entries.len(), lines.len() - 1, "{scenario}: entry count");
        assert_eq!(sm.get_tree().len(), 1, "{scenario}: single-root tree");

        // Context contains exactly the message entries.
        let ctx = sm.build_session_context();
        assert_eq!(
            ctx.messages.len(),
            message_lines,
            "{scenario}: context messages"
        );

        // Lossless export: normalized diff against the original fixture.
        let exported = sm.export_jsonl().expect("export");
        let exported_header: Value =
            serde_json::from_str(non_empty_lines(&exported)[0]).expect("exported header parses");
        assert_eq!(
            exported_header["version"],
            Value::from(3),
            "{scenario}: header v3"
        );
        diff_jsonl(&original, &exported)
            .unwrap_or_else(|f| panic!("{scenario}: export parity diff:\n{f}"));
    }
}

#[test]
fn parity_fixture_sessions_continue_after_load() {
    for scenario in SCENARIOS {
        let (_dir, staged, original) = stage_fixture(scenario);
        let original_line_count = non_empty_lines(&original).len();

        let mut sm = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: open: {e}"));
        let before_ctx = sm.build_session_context();
        let before_entries = sm.get_entries().len();

        sm.append_message(user_msg("continued question"))
            .expect("append user");
        sm.append_message(AgentMessage::Assistant(faux_assistant_message(
            "continued answer",
            FauxAssistantOptions::default(),
        )))
        .expect("append assistant");

        // Context keeps the old messages, then the new pair.
        let after_ctx = sm.build_session_context();
        assert_eq!(
            after_ctx.messages.len(),
            before_ctx.messages.len() + 2,
            "{scenario}: context grows by the appended pair"
        );
        assert_eq!(
            after_ctx.messages[..before_ctx.messages.len()],
            before_ctx.messages[..],
            "{scenario}: old context messages unchanged"
        );
        assert_eq!(
            sm.get_entries().len(),
            before_entries + 2,
            "{scenario}: entries"
        );

        // The file gained exactly two lines.
        let on_disk = std::fs::read_to_string(&staged).expect("read staged file");
        assert_eq!(
            non_empty_lines(&on_disk).len(),
            original_line_count + 2,
            "{scenario}: file appended"
        );

        // Reopen: full state survives a reload.
        let reopened = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: reopen: {e}"));
        assert_eq!(
            reopened.get_entries().len(),
            before_entries + 2,
            "{scenario}: reload entries"
        );
        let reopened_ctx = reopened.build_session_context();
        assert_eq!(
            reopened_ctx.messages.len(),
            before_ctx.messages.len() + 2,
            "{scenario}: reload context"
        );
    }
}

/// W8 session 互通终验：上游实录 fixture 加载进全栈 `AgentSession`（faux
/// provider），经真实 `prompt()` 续跑一轮——文件恰好追加 user+assistant
/// 两条 message 行、原有行逐字节不动、重开后 context 含旧消息 + 新回合。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parity_fixture_session_prompt_continue_with_faux_provider() {
    use rpi::core::agent_session::PromptOptions;
    use rpi::core::agent_session_services::{
        create_agent_session_services, CreateAgentSessionServicesOptions,
    };
    use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
    use rpi::sdk::{create_agent_session, CreateAgentSessionOptions};
    use rpi_test_support::faux::{FauxAiProvider, FauxProvider, FauxProviderOptions};
    use std::sync::{Arc, Mutex};

    let (dir, staged, original) = stage_fixture("single-turn");
    let cwd = dir.0.join("workspace");
    let agent_dir = dir.0.join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let provider = FauxProvider::new(FauxProviderOptions::default());
    provider.set_responses(vec![faux_assistant_message(
        "continued answer",
        FauxAssistantOptions::default(),
    )
    .into()]);
    let model = provider.get_model(None).expect("faux model");

    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: None,
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
        ..Default::default()
    })
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
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

    // cwd_override：fixture header 的 cwd 是上游实录机的 /tmp 路径（本机不存在）。
    let session_manager =
        SessionManager::open(&staged, None, Some(&cwd)).expect("open fixture session");
    let before_lines = non_empty_lines(&original);

    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
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
        .prompt("continued question", PromptOptions::default())
        .await
        .expect("prompt");

    // 文件恰好追加两行（同 model/thinking 不变，无额外条目），前缀不动。
    let on_disk = std::fs::read_to_string(&staged).expect("read continued session");
    let continued_lines = non_empty_lines(&on_disk);
    assert_eq!(
        continued_lines.len(),
        before_lines.len() + 2,
        "file appended exactly the continued turn"
    );
    assert_eq!(
        continued_lines[..before_lines.len()],
        before_lines[..],
        "original fixture lines untouched"
    );

    // 重开：旧消息完整，末条是 faux 续跑的 assistant 回复。
    let reopened = SessionManager::open(&staged, None, Some(&dir.0.join("workspace")))
        .expect("reopen continued session");
    let ctx = reopened.build_session_context();
    let last = ctx.messages.last().expect("continued assistant message");
    let last_json = serde_json::to_value(last).expect("serialize last message");
    assert_eq!(
        last_json.get("role").and_then(Value::as_str),
        Some("assistant"),
        "last context message is the continued assistant reply"
    );
    assert!(
        last_json.to_string().contains("continued answer"),
        "continued reply content present: {last_json}"
    );
}
