//! Agent-event payload forwarding tests (HR-A / HR-B / HR-C,
//! rpi-docs/extensions/rpi-statusline/03-realtime-token-count §1.3):
//! the 8 agent events that upstream `_emitExtensionEvent`
//! (agent-session.ts:727-808) forwards WITH payloads must reach extension
//! handlers in the exact upstream shapes — `message_update` carries
//! `message` + `assistantMessageEvent`, `turn_start` carries `turnIndex` +
//! unix-ms `timestamp`, tool events carry `toolCallId`/`toolName`/… .
//!
//! Full-stack chain per case: faux provider → Agent loop →
//! `AgentSession::handle_agent_event` → `emit_extension_event` →
//! ExtensionHostAdapter → NativeExtensionHost → inline handler.
//!
//! Also covers the `ctx.sessionFile` additive host-call (HR-E / ADR-0022)
//! round trip through the session-bound `ContextActions`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use rpi_ext_host::api::ContextActions;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rpi-agent-payload-{id}-{}", std::process::id()));
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

/// A custom tool that reports one partial update mid-execution (drives
/// `tool_execution_update`) and then returns a plain result.
struct UpdatingTool;

#[async_trait::async_trait]
impl AgentTool for UpdatingTool {
    fn name(&self) -> &str {
        "updating"
    }

    fn label(&self) -> &str {
        "updating"
    }

    fn description(&self) -> &str {
        "reports a partial update"
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMETERS.get_or_init(|| json!({"type": "object"}))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: tokio_util::sync::CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        if let Some(on_update) = on_update {
            on_update(AgentToolResult {
                content: vec![rpi_ai::types::ToolResultContent::Text(
                    rpi_ai::types::TextContent {
                        text: "partial".to_owned(),
                        text_signature: None,
                    },
                )],
                ..Default::default()
            });
        }
        Ok(AgentToolResult {
            content: vec![rpi_ai::types::ToolResultContent::Text(
                rpi_ai::types::TextContent {
                    text: format!("final {params}"),
                    text_signature: None,
                },
            )],
            ..Default::default()
        })
    }
}

/// Recorded agent-event payloads: `(event, payload)` in arrival order.
type Records = Arc<Mutex<Vec<(String, Value)>>>;

fn recording_ext(records: Records, events: &[&'static str]) -> InlineExtension {
    let events = events.to_vec();
    let factory: ExtensionFactory = Arc::new(move |api| {
        for event in events.iter().copied() {
            let records = records.clone();
            api.on(
                event,
                Arc::new(move |payload, _ctx| {
                    records
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((event.to_string(), payload));
                    Box::pin(async { Ok(Value::Null) })
                }),
            )
            .expect("register handler");
        }
        Box::pin(async { Ok(()) })
    });
    InlineExtension::Anonymous(factory)
}

const AGENT_EVENTS: [&str; 8] = [
    "agent_end",
    "turn_start",
    "turn_end",
    "message_start",
    "message_update",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
];

struct Fixture {
    session: rpi::core::agent_session::AgentSession,
    records: Records,
    _tmp: TempDir,
}

/// Full-stack session (w2-test shape): faux provider scripted with a
/// tool-call turn followed by a plain text turn; inline extension records
/// the agent-event payloads.
async fn fixture(responses: Vec<FauxResponseStep>, host: NativeExtensionHost) -> Fixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

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
    provider.set_responses(responses);
    let model = provider.get_model(None).expect("faux model");

    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(
        rpi::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: rpi::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
            ..Default::default()
        },
    )
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux provider");

    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");

    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));

    let host = Arc::new(host);
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        tools: Some(vec!["updating".to_owned()]),
        custom_tools: vec![Arc::new(UpdatingTool)],
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host),
        ..Default::default()
    })
    .await
    .expect("create session");

    Fixture {
        session: created.session,
        records: Arc::new(Mutex::new(Vec::new())),
        _tmp: tmp,
    }
}

fn tool_call_step() -> FauxResponseStep {
    faux_assistant_message(
        faux_tool_call(
            "updating",
            json!({"n": 1}).as_object().cloned().unwrap(),
            None,
        ),
        FauxAssistantOptions::default(),
    )
    .into()
}

fn text_step(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

fn records_of(records: &Records, event: &str) -> Vec<Value> {
    records
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(name, _)| name == event)
        .map(|(_, payload)| payload.clone())
        .collect()
}

async fn run_prompt(fixture: &Fixture) {
    fixture
        .session
        .prompt("go", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;
    // Extension dispatch is awaited inside agent-event handling; give the
    // recording mutex a beat to settle for the tail events.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

/// A1: all 8 payloads anchor 1:1 against the §1.3 table
/// (upstream agent-session.ts:727-808 shapes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_event_payloads_match_upstream_shapes() {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let host = NativeExtensionHost::new("/payload-cwd");
    let errors = host
        .load_inline(&[recording_ext(records.clone(), &AGENT_EVENTS)])
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let mut fixture_holder = fixture(vec![tool_call_step(), text_step("all done")], host).await;
    // Rebind the fixture's records slot: the fixture's own field is unused
    // for this scenario; the extension's records are authoritative.
    let _ = std::mem::take(&mut fixture_holder.records);
    run_prompt(&fixture_holder).await;

    // agent_end: {type, messages}; the last message is the final assistant.
    let agent_ends = records_of(&records, "agent_end");
    assert_eq!(agent_ends.len(), 1, "one agent_end per prompt");
    let agent_end = &agent_ends[0];
    assert_eq!(agent_end["type"], "agent_end");
    let messages = agent_end["messages"].as_array().expect("messages array");
    let last = messages.last().expect("at least one message");
    assert_eq!(last["role"], "assistant");

    // turn_start: {type, turnIndex, timestamp(unix ms)} — HR-B sequence
    // 0, 1 (tool turn, then final turn).
    let turn_starts = records_of(&records, "turn_start");
    assert_eq!(turn_starts.len(), 2, "two turns");
    let now_ms = rpi_ai::models::now_millis();
    for (index, payload) in turn_starts.iter().enumerate() {
        assert_eq!(payload["type"], "turn_start");
        assert_eq!(payload["turnIndex"], index as u64);
        let timestamp = payload["timestamp"].as_i64().expect("timestamp");
        assert!(
            (now_ms - timestamp).abs() < 60_000,
            "unix-ms timestamp near now: {timestamp}"
        );
    }

    // turn_end: {type, turnIndex, message, toolResults} — turn 0 carries
    // the tool result, turn 1 has none.
    let turn_ends = records_of(&records, "turn_end");
    assert_eq!(turn_ends.len(), 2);
    assert_eq!(turn_ends[0]["type"], "turn_end");
    assert_eq!(turn_ends[0]["turnIndex"], 0);
    assert_eq!(turn_ends[0]["message"]["role"], "assistant");
    assert_eq!(
        turn_ends[0]["toolResults"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(turn_ends[1]["turnIndex"], 1);
    assert_eq!(
        turn_ends[1]["toolResults"].as_array().map(Vec::len),
        Some(0)
    );

    // message_start: {type, message} — assistant role for stream turns.
    let message_starts = records_of(&records, "message_start");
    assert!(message_starts.len() >= 3, "user + two assistant starts");
    assert_eq!(message_starts[0]["type"], "message_start");
    assert_eq!(message_starts[0]["message"]["role"], "user");
    assert_eq!(message_starts[1]["message"]["role"], "assistant");

    // message_update: {type, message, assistantMessageEvent} — the faux
    // stream emits text deltas for the plain-text turn.
    let updates = records_of(&records, "message_update");
    assert!(!updates.is_empty(), "streaming deltas forwarded");
    for payload in &updates {
        assert_eq!(payload["type"], "message_update");
        assert_eq!(payload["message"]["role"], "assistant");
        assert!(
            payload["assistantMessageEvent"].is_object(),
            "assistantMessageEvent object"
        );
    }
    assert!(
        updates.iter().any(|payload| payload
            .pointer("/assistantMessageEvent/type")
            .and_then(Value::as_str)
            == Some("text_delta")),
        "text_delta events present: {updates:#?}"
    );
    let sample = updates
        .iter()
        .find(|payload| {
            payload
                .pointer("/assistantMessageEvent/type")
                .and_then(Value::as_str)
                == Some("text_delta")
        })
        .expect("a text_delta update");
    assert!(
        sample["assistantMessageEvent"]["delta"].is_string(),
        "delta string present"
    );

    // tool_execution_start: {type, toolCallId, toolName, args}.
    let tool_starts = records_of(&records, "tool_execution_start");
    assert_eq!(tool_starts.len(), 1);
    assert_eq!(tool_starts[0]["type"], "tool_execution_start");
    assert_eq!(tool_starts[0]["toolName"], "updating");
    assert!(!tool_starts[0]["toolCallId"]
        .as_str()
        .is_some_and(str::is_empty));
    assert_eq!(tool_starts[0]["args"], json!({"n": 1}));

    // tool_execution_update: {type, toolCallId, toolName, args, partialResult}.
    let tool_updates = records_of(&records, "tool_execution_update");
    assert_eq!(tool_updates.len(), 1, "one partial from UpdatingTool");
    assert_eq!(tool_updates[0]["type"], "tool_execution_update");
    assert_eq!(tool_updates[0]["toolName"], "updating");
    assert_eq!(
        tool_updates[0]["partialResult"]["content"][0]["text"], "partial",
        "partialResult carries the AgentToolResult JSON"
    );

    // tool_execution_end: {type, toolCallId, toolName, result, isError}.
    let tool_ends = records_of(&records, "tool_execution_end");
    assert_eq!(tool_ends.len(), 1);
    assert_eq!(tool_ends[0]["type"], "tool_execution_end");
    assert_eq!(tool_ends[0]["toolName"], "updating");
    assert_eq!(tool_ends[0]["isError"], false);
    assert!(tool_ends[0]["result"].is_object(), "result object");
}

/// HR-B: `turnIndex` resets to 0 on every `agent_start` — a second prompt
/// restarts the sequence (agent-session.ts:727/748).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_index_resets_per_agent_start() {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let host = NativeExtensionHost::new("/payload-cwd");
    let errors = host
        .load_inline(&[recording_ext(records.clone(), &["turn_start", "turn_end"])])
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let fixture_holder = fixture(vec![text_step("first"), text_step("second")], host).await;
    run_prompt(&fixture_holder).await;
    run_prompt(&fixture_holder).await;

    let turn_starts = records_of(&records, "turn_start");
    assert_eq!(turn_starts.len(), 2);
    assert_eq!(turn_starts[0]["turnIndex"], 0);
    assert_eq!(turn_starts[1]["turnIndex"], 0, "reset per agent_start");
    let turn_ends = records_of(&records, "turn_end");
    assert_eq!(turn_ends[0]["turnIndex"], 0);
    assert_eq!(turn_ends[1]["turnIndex"], 0);
}

/// A2 / HR-C: with no extension subscribed to the agent events, the
/// forwarding short-circuits before payload construction — nothing is
/// dispatched and the run itself is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsubscribed_agent_events_short_circuit() {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let host = NativeExtensionHost::new("/payload-cwd");
    // Subscribe ONLY to an event the agent loop never emits here.
    let errors = host
        .load_inline(&[recording_ext(records.clone(), &["model_select"])])
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let fixture_holder = fixture(vec![tool_call_step(), text_step("done")], host).await;
    run_prompt(&fixture_holder).await;

    let all = records.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        all.is_empty(),
        "no agent-event payload may reach a non-subscriber: {all:#?}"
    );
    // And the run itself completed normally.
    let last_text = fixture_holder
        .session
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            rpi_agent::messages::AgentMessage::Assistant(assistant) => {
                let text = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        rpi_ai::types::AssistantContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<String>();
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
        .next_back()
        .expect("assistant text");
    assert_eq!(last_text, "done");
}

/// HR-E / ADR-0022: `ctx.sessionFile` is the authoritative session identity
/// — file-backed sessions answer `{path, id}`, in-memory sessions answer
/// `{path: null, id}`. Both flow through the session-bound
/// `ContextActions` used by the host-call dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctx_session_file_round_trip() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

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
    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(
        rpi::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: rpi::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
            ..Default::default()
        },
    )
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux provider");
    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");

    // File-backed session: create writes the header file eagerly.
    let session_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let file_session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::create(
            &cwd,
            Some(&session_dir),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("file-backed session"),
    ));
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd.clone()),
        agent_dir: Some(tmp.path().join("agent")),
        model_runtime: Some(model_runtime.clone()),
        model: Some(model.clone()),
        services: Some(services),
        session_manager: Some(file_session_manager),
        ..Default::default()
    })
    .await
    .expect("create session");

    let actions = rpi::core::extension_context::SessionContextActions::new(&created.session);
    let info = actions
        .get_session_file()
        .expect("bound actions answer with the authoritative identity");
    let path = info.path.expect("file-backed session has a path");
    let recorded = created
        .session
        .session_file()
        .expect("session file recorded")
        .display()
        .to_string();
    assert_eq!(Path::new(&path), Path::new(&recorded));
    assert_eq!(info.id, created.session.session_id());
    assert!(path.ends_with(".jsonl"));

    // In-memory session: path is null, id still present.
    let memory_session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));
    let memory_created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(tmp.path().join("agent-2")),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: None,
        session_manager: Some(memory_session_manager),
        ..Default::default()
    })
    .await
    .expect("create in-memory session");
    let actions = rpi::core::extension_context::SessionContextActions::new(&memory_created.session);
    let info = actions
        .get_session_file()
        .expect("bound in-memory actions answer");
    assert_eq!(info.path, None);
    assert_eq!(info.id, memory_created.session.session_id());
    assert!(!info.id.is_empty());
}
