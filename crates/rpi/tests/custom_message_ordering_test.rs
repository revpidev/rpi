//! V14-11 FR-A/FR-B: run-time custom messages land after the turn's tool
//! results (upstream #8537, `240eb29c4`; trigger gating `47b5119d0`).
//!
//! Port of `test/suite/regressions/8537-custom-message-tool-result-ordering.test.ts`
//! intents:
//! - a custom message sent with `triggerTurn: false` during tool execution
//!   is NOT steered, NOT appended mid-run, and emits no message events
//!   until the turn's tool results are in state and session;
//! - it is appended (tree + events) after the tool result and before the
//!   next assistant message;
//! - the provider-visible request order never places it between an
//!   assistant tool call and its tool result.

use std::sync::{Arc, Mutex, OnceLock};

use rpi_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use rpi_ai::types::Context;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rpi-v1411-ordering-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A tool that, mid-execution, delivers a custom message to the session
/// (a background task notifying the session while the tool still runs) and
/// then blocks until the test releases it.
struct NotifyingTool {
    session: OnceLock<rpi::core::agent_session::AgentSession>,
    trigger_turn: Option<bool>,
    /// Signalled after the send returned (test asserts mid-run state).
    sent: mpsc::UnboundedSender<()>,
    /// The test releases the tool through this channel after asserting.
    release: Mutex<Option<mpsc::UnboundedReceiver<()>>>,
}

#[async_trait::async_trait]
impl AgentTool for NotifyingTool {
    fn name(&self) -> &str {
        "wait"
    }

    fn label(&self) -> &str {
        "Wait"
    }

    fn description(&self) -> &str {
        "Wait for a background task"
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMETERS.get_or_init(|| json!({"type": "object"}))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        let session = self.session.get().expect("session set").clone();
        session
            .send_custom_message(
                "subagent-reply",
                Some(rpi_ai::types::UserContent::Text(
                    "subagent replied".to_owned(),
                )),
                true,
                None,
                self.trigger_turn,
                None,
            )
            .await
            .expect("send_custom_message");
        self.sent.send(()).expect("sent signal");
        let release = self
            .release
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(mut release) = release {
            let _ = release.recv().await;
        }
        Ok(AgentToolResult {
            content: vec![rpi_ai::types::ToolResultContent::Text(
                rpi_ai::types::TextContent {
                    text: "tool done".to_owned(),
                    text_signature: None,
                },
            )],
            ..Default::default()
        })
    }
}

/// Captured per-call request message roles (the faux `Factory` step).
type SeenContexts = Arc<Mutex<Vec<String>>>;

fn tool_call_step(name: &str) -> FauxResponseStep {
    faux_assistant_message(
        vec![faux_tool_call(
            name,
            json!({}).as_object().cloned().unwrap(),
            None,
        )],
        FauxAssistantOptions {
            stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
            ..Default::default()
        },
    )
    .into()
}

/// Second-response factory that records the request message roles.
fn recording_step(seen: SeenContexts, text: &str) -> FauxResponseStep {
    let text = text.to_owned();
    FauxResponseStep::Factory(Box::new(move |context: &Context, _opts, _state, _model| {
        seen.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(context_roles(context).join(","));
        faux_assistant_message(text.clone(), FauxAssistantOptions::default())
    }))
}

fn context_roles(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .map(|message| {
            serde_json::to_value(message)
                .ok()
                .and_then(|v| v.get("role").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "?".to_owned())
        })
        .collect()
}

fn session_roles(session: &rpi::core::agent_session::AgentSession) -> Vec<String> {
    session
        .messages()
        .iter()
        .map(|message| {
            serde_json::to_value(message)
                .ok()
                .and_then(|v| v.get("role").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "?".to_owned())
        })
        .collect()
}

/// Entry kinds in branch order, upstream `entryKinds` assertion shape:
/// message entries flatten to their message role, custom messages to
/// `"custom"` (8537 test, intent 2).
fn entry_kinds(entries: Vec<rpi_agent::session::SessionEntry>) -> Vec<String> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let value = serde_json::to_value(&entry).ok()?;
            match value.get("type")?.as_str()? {
                "message" => value
                    .pointer("/message/role")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                "custom_message" => Some("custom".to_owned()),
                _ => None,
            }
        })
        .collect()
}

struct Fixture {
    session: rpi::core::agent_session::AgentSession,
    provider: Arc<FauxProvider>,
    seen: SeenContexts,
    _tmp: TempDir,
}

/// Full pipeline (w3-test shape) with the notifying tool registered as a
/// custom tool; responses: tool-call turn → recorded text turn.
async fn fixture(tool: Arc<NotifyingTool>, tag: &str) -> Fixture {
    let tmp = TempDir::new(tag);
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let seen: SeenContexts = Arc::new(Mutex::new(Vec::new()));
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
    provider.set_responses(vec![
        tool_call_step("wait"),
        recording_step(seen.clone(), "done"),
    ]);
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

    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        tools: Some(vec!["wait".to_owned()]),
        custom_tools: vec![tool.clone()],
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host),
        ..Default::default()
    })
    .await
    .expect("create session");

    tool.session.set(created.session.clone()).ok();

    Fixture {
        session: created.session,
        provider,
        seen,
        _tmp: tmp,
    }
}

/// Append more scripted responses (multi-prompt scenarios).
fn provider_append(fixture: &Fixture, responses: Vec<FauxResponseStep>) {
    fixture.provider.append_responses(responses);
}

/// `triggerTurn: false` while streaming → queue, append after tool results
/// (upstream 8537 regression, intents 1-3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trigger_turn_false_during_run_appends_after_tool_results() {
    let (sent_tx, mut sent_rx) = mpsc::unbounded_channel::<()>();
    let (release_tx, release_rx) = mpsc::unbounded_channel::<()>();
    let tool = Arc::new(NotifyingTool {
        session: OnceLock::new(),
        trigger_turn: Some(false),
        sent: sent_tx,
        release: Mutex::new(Some(release_rx)),
    });
    let fixture = fixture(tool.clone(), "false").await;

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let event_log = events.clone();
    let _unsubscribe = fixture.session.subscribe(Arc::new(move |event| {
        if let rpi::core::agent_session::AgentSessionEvent::Agent(agent_event) = event {
            if let rpi_agent::types::AgentEvent::MessageStart { message } = agent_event.as_ref() {
                let role = serde_json::to_value(message)
                    .ok()
                    .and_then(|v| v.get("role").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                event_log
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(role);
            }
        }
    }));

    let session = fixture.session.clone();
    let prompt_task = tokio::spawn(async move {
        session
            .prompt("hi", rpi::core::agent_session::PromptOptions::default())
            .await
            .expect("prompt");
    });

    // Tool delivered the message and is blocked: mid-run state check.
    sent_rx.recv().await.expect("tool sent signal");
    assert!(
        fixture
            .session
            .messages()
            .iter()
            .all(|m| !matches!(m, rpi_agent::messages::AgentMessage::Custom(_))),
        "queued message must not be in agent state mid-run"
    );
    assert!(
        !events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|role| role == "custom"),
        "no message events for a message the tree does not contain"
    );

    release_tx.send(()).expect("release tool");
    prompt_task.await.expect("prompt task");
    fixture.session.wait_for_idle().await;

    // Post-run: tree order (upstream roles assertion).
    assert_eq!(
        session_roles(&fixture.session),
        vec!["user", "assistant", "toolResult", "custom", "assistant"],
    );
    // Session entries keep the same order (upstream entryKinds).
    let kinds = entry_kinds(
        fixture
            .session
            .session_manager()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_branch(None)
            .into_iter()
            .filter_map(|stored| stored.known().cloned())
            .collect(),
    );
    assert_eq!(
        kinds,
        vec!["user", "assistant", "toolResult", "custom", "assistant"],
        "entry kinds: {kinds:?}"
    );
    // message events mirror the tree.
    assert_eq!(
        *events.lock().unwrap_or_else(|e| e.into_inner()),
        vec!["user", "assistant", "toolResult", "custom", "assistant"],
    );

    // Provider-visible order (upstream intent 3): the queued message did
    // NOT enter the in-flight run's request (agent-loop keeps its own
    // context; the flush lands in session state at turn_end).
    let seen = fixture
        .seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(seen.len(), 1, "exactly one post-tool request: {seen:?}");
    assert_eq!(seen[0], "user,assistant,toolResult", "request roles");

    // Second prompt (upstream intent 3): the queued message joins the
    // request built from session state, AFTER the tool result — the
    // tool-call/tool-result pairing stays intact.
    provider_append(
        &fixture,
        vec![recording_step(fixture.seen.clone(), "second turn")],
    );
    fixture
        .session
        .prompt(
            "and now?",
            rpi::core::agent_session::PromptOptions::default(),
        )
        .await
        .expect("second prompt");
    fixture.session.wait_for_idle().await;

    let seen = fixture
        .seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(seen.len(), 2, "two recorded requests: {seen:?}");
    assert_eq!(
        seen[1], "user,assistant,toolResult,user,assistant,user",
        "second prompt request roles (custom→user after toolResult): {}",
        seen[1]
    );
}

/// `triggerTurn` absent while streaming → still steers (FR-A; the
/// `!== false` gate, 47b5119d0). Observable: the custom message is part of
/// the SAME run's delivered context, never dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trigger_turn_absent_during_run_still_steers() {
    let (sent_tx, mut sent_rx) = mpsc::unbounded_channel::<()>();
    let (release_tx, release_rx) = mpsc::unbounded_channel::<()>();
    let tool = Arc::new(NotifyingTool {
        session: OnceLock::new(),
        trigger_turn: None,
        sent: sent_tx,
        release: Mutex::new(Some(release_rx)),
    });
    let fixture = fixture(tool.clone(), "absent").await;

    let session = fixture.session.clone();
    let prompt_task = tokio::spawn(async move {
        session
            .prompt("hi", rpi::core::agent_session::PromptOptions::default())
            .await
            .expect("prompt");
    });

    sent_rx.recv().await.expect("tool sent signal");
    release_tx.send(()).expect("release tool");
    prompt_task.await.expect("prompt task");
    fixture.session.wait_for_idle().await;

    // The steered custom message reached the LLM in the SAME run (steer
    // path — the `!== false` gate): trailing user-role context message
    // after the tool result, unlike the queued (Some(false)) variant.
    let seen = fixture
        .seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(seen.len(), 1, "second request happened: {seen:?}");
    assert_eq!(
        seen[0], "user,assistant,toolResult,user",
        "steered message in request: {}",
        seen[0]
    );
    // And it landed in the final tree exactly once.
    assert_eq!(
        session_roles(&fixture.session)
            .iter()
            .filter(|role| *role == "custom")
            .count(),
        1
    );
}
