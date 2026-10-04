//! Virtual models (V16-12, R3.11): registration, per-request routing,
//! router state, limits, and restore.
//!
//! Port of `packages/coding-agent/test/virtual-models.test.ts` and
//! `test/suite/virtual-models.test.ts` @ a13d35a74 (the router is a native
//! inline extension; the wasm carrier is covered by the `rpi-ext-host` /
//! `rpi-ext-sdk` host-call tests).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi::core::agent_session::{AgentSession, PromptOptions};
use rpi::core::extension_actions::bind_session_actions;
use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_ext_host::types as ext;
use rpi_ext_host::types::VirtualModelRouteFn;
use rpi_test_support::faux::{
    FauxAiProvider, FauxAssistantOptions, FauxContent, FauxModelDefinition, FauxProvider,
    FauxProviderOptions, FauxResponseStep, faux_assistant_message, faux_text, faux_tool_call,
};
use serde_json::{Value, json};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rpi-vm-test-{}-{id}", std::process::id()));
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

/// What the test router does for a request, and what it observed.
#[derive(Default)]
struct RouterControl {
    reasons: Mutex<Vec<String>>,
    previous_ids: Mutex<Vec<Option<String>>>,
    failed_present: Mutex<Vec<bool>>,
    state_present: Mutex<Vec<bool>>,
    calls: AtomicU64,
    fail: AtomicBool,
    route_to_virtual: AtomicBool,
    route_to_missing: AtomicBool,
    route_to_unconfigured: AtomicBool,
}

impl RouterControl {
    fn reasons(&self) -> Vec<String> {
        self.reasons
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn previous_ids(&self) -> Vec<Option<String>> {
        self.previous_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn failed_present(&self) -> Vec<bool> {
        self.failed_present
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn state_present(&self) -> Vec<bool> {
        self.state_present
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// A tool call since the last user message edited a file successfully.
fn edited_this_turn(request: &Value) -> bool {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return false;
    };
    let last_user = messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|index| index + 1)
        .unwrap_or(0);
    messages[last_user..].iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("toolResult")
            && message
                .get("toolName")
                .and_then(Value::as_str)
                .is_some_and(|name| name == "edit")
            && message.get("isError").and_then(Value::as_bool) != Some(true)
    })
}

fn route(control: Arc<RouterControl>, request: Value) -> Result<Value, String> {
    control.calls.fetch_add(1, Ordering::SeqCst);
    control
        .reasons
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(
            request
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        );
    control
        .previous_ids
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(
            request
                .pointer("/previous/model/id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
    control
        .failed_present
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(request.get("failed").is_some_and(|value| !value.is_null()));
    control
        .state_present
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(request.get("state").is_some_and(|value| !value.is_null()));
    if control.fail.load(Ordering::SeqCst) {
        return Err("router unavailable".to_owned());
    }
    if control.route_to_virtual.load(Ordering::SeqCst) {
        return Ok(json!({
            "model": { "provider": "router", "id": "auto" },
            "thinkingLevel": "off",
        }));
    }
    if control.route_to_missing.load(Ordering::SeqCst) {
        return Ok(json!({
            "model": { "provider": "faux", "id": "missing-1" },
            "thinkingLevel": "off",
        }));
    }
    if control.route_to_unconfigured.load(Ordering::SeqCst) {
        return Ok(json!({
            "model": { "provider": "noauth", "id": "na-1" },
            "thinkingLevel": "off",
        }));
    }
    // `direct` requests (compaction summaries, extension calls) always go to
    // the implementation model, like upstream's Jev example.
    if request.get("reason").and_then(Value::as_str) == Some("direct") {
        return Ok(json!({
            "model": { "provider": "faux", "id": "impl-1" },
            "thinkingLevel": request.get("thinkingLevel").cloned().unwrap_or(json!("off")),
        }));
    }
    let state = request.get("state").filter(|state| !state.is_null());
    let model = match state
        .and_then(|state| state.get("phase"))
        .and_then(Value::as_str)
    {
        None => {
            if edited_this_turn(&request) {
                "impl-1"
            } else {
                "plan-1"
            }
        }
        Some("planning") => {
            if edited_this_turn(&request) {
                "impl-1"
            } else {
                "plan-1"
            }
        }
        Some(_) => "impl-1",
    };
    let phase = if model == "impl-1" {
        "implementation"
    } else {
        "planning"
    };
    Ok(json!({
        "model": { "provider": "faux", "id": model },
        "thinkingLevel": request.get("thinkingLevel").cloned().unwrap_or(json!("off")),
        "state": { "phase": phase, "model": model },
    }))
}

/// A stub `edit` tool that reports success without touching the filesystem:
/// the router's phase switch keys on a successful `edit`/`write` tool result.
fn stub_edit_tool() -> ext::ToolDefinition {
    ext::ToolDefinition {
        name: "edit".to_owned(),
        label: "Edit".to_owned(),
        description: "Stub edit tool".to_owned(),
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
        output_schema: None,
        exposure: ext::ToolExposure::Direct,
        namespace: None,
        annotations: None,
        default_active: None,
        prepare_loadout: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_request, _ctx| {
            Box::pin(async move {
                Ok(rpi_agent::types::AgentToolResult {
                    content: vec![rpi_ai::types::ToolResultContent::Text(
                        rpi_ai::types::TextContent {
                            text: "edited".to_owned(),
                            text_signature: None,
                        },
                    )],
                    details: json!({}),
                    ..Default::default()
                })
            })
        }),
        render_call: None,
        render_result: None,
    }
}

fn router_extension(control: Arc<RouterControl>) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        let control = control.clone();
        Box::pin(async move {
            api.register_tool(stub_edit_tool())
                .map_err(|error| error.to_string())?;
            let route_fn: VirtualModelRouteFn = Arc::new(move |request: Value| {
                let control = control.clone();
                Box::pin(async move { route(control, request) })
            });
            api.register_virtual_model(
                json!({
                    "provider": "router",
                    "id": "auto",
                    "name": "Auto (test)",
                    "thinkingLevels": ["off", "low", "medium", "high"],
                    // Deliberately small; the physical model that answers
                    // supplies the real limits afterwards.
                    "contextWindow": 1_000,
                    "maxTokens": 1_000,
                }),
                route_fn,
            )
            .await
            .map_err(|error| error.to_string())
        })
    });
    InlineExtension::Anonymous(factory)
}

struct Fixture {
    session: AgentSession,
    provider: Arc<FauxProvider>,
    ai_provider: Arc<FauxAiProvider>,
    control: Arc<RouterControl>,
    _tmp: TempDir,
}

/// How the fixture picks the initial model.
enum InitialModel {
    /// The faux planning model (default).
    Default,
    /// No explicit model: the SDK restore path decides.
    None,
}

impl Fixture {
    fn last_assistant(&self) -> rpi_ai::types::AssistantMessage {
        self.session
            .messages()
            .into_iter()
            .rev()
            .find_map(|message| match message {
                rpi_agent::AgentMessage::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .expect("assistant message")
    }

    fn state_entries(&self) -> Vec<Value> {
        self.session
            .session_manager()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_branch(None)
            .iter()
            .filter_map(|entry| entry.known().cloned())
            .filter_map(|entry| match entry {
                rpi_agent::session::SessionEntry::Custom(custom)
                    if custom.custom_type == "pi.virtual-model-state" =>
                {
                    custom.data
                }
                _ => None,
            })
            .collect()
    }
}

/// Build a session over a faux provider with the router extension loaded.
/// `retry` writes a zero-delay retry settings file; `with_router` controls
/// whether the virtual model is registered (resume fallback tests).
async fn session_fixture(
    retry: bool,
    with_router: bool,
    initial_model: InitialModel,
    manager: Option<rpi::core::session_manager::SessionManager>,
) -> Fixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let settings = if retry {
        r#"{"retry": {"enabled": true, "maxRetries": 3, "baseDelayMs": 0}, "compaction": {"enabled": true, "reserveTokens": 0, "keepRecentTokens": 1}}"#
    } else {
        r#"{"compaction": {"enabled": true, "reserveTokens": 0, "keepRecentTokens": 1}}"#
    };
    std::fs::write(agent_dir.join("settings.json"), settings).expect("write settings");
    // Unconfigured provider: the api key env var is never set, so the
    // provider composes but never becomes available.
    std::fs::write(
        agent_dir.join("models.json"),
        r#"{"providers": {"noauth": {
            "baseUrl": "https://noauth.example.com/v1",
            "api": "openai-completions",
            "apiKey": "${RPI_TEST_VM_MISSING_KEY}",
            "models": [{"id": "na-1", "contextWindow": 1000}]
        }}}"#,
    )
    .expect("write models.json");

    let provider = FauxProvider::new(FauxProviderOptions {
        provider: Some("faux".to_owned()),
        models: Some(vec![
            FauxModelDefinition {
                id: "plan-1".to_owned(),
                name: None,
                reasoning: Some(true),
                input: None,
                input_limits: None,
                cost: None,
                context_window: Some(50_000),
                max_tokens: Some(8192),
            },
            FauxModelDefinition {
                id: "impl-1".to_owned(),
                name: None,
                reasoning: Some(true),
                input: None,
                input_limits: None,
                cost: None,
                context_window: Some(10_000),
                max_tokens: Some(8192),
            },
            FauxModelDefinition {
                id: "plain-1".to_owned(),
                name: None,
                reasoning: Some(false),
                input: None,
                input_limits: None,
                cost: None,
                context_window: Some(10_000),
                max_tokens: Some(8192),
            },
        ]),
        ..Default::default()
    });
    let ai_provider = Arc::new(FauxAiProvider::new(provider.clone()));
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
        .register_native_provider(ai_provider.clone() as Arc<dyn rpi_ai::models::Provider>)
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

    let control = Arc::new(RouterControl::default());
    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    if with_router {
        let errors = host.load_inline(&[router_extension(control.clone())]).await;
        assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    }

    let session_manager = Arc::new(Mutex::new(match manager {
        Some(manager) => manager,
        None => rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    }));

    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: match initial_model {
            InitialModel::Default => Some(provider.get_model(Some("plan-1")).expect("plan model")),
            InitialModel::None => None,
        },
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host.clone()),
        exclude_tools: Some(vec![
            "read".to_owned(),
            "bash".to_owned(),
            "write".to_owned(),
        ]),
        ..Default::default()
    })
    .await
    .expect("create session");

    bind_session_actions(&host, &created.session).await;

    Fixture {
        session: created.session,
        provider,
        ai_provider,
        control,
        _tmp: tmp,
    }
}

fn assistant_reply(text: &str) -> FauxResponseStep {
    FauxResponseStep::Message(Box::new(faux_assistant_message(
        FauxContent(vec![faux_text(text)]),
        FauxAssistantOptions::default(),
    )))
}

/// A reply factory that records the model the request was streamed with.
fn recording_reply(models: Arc<Mutex<Vec<String>>>, text: &str) -> FauxResponseStep {
    let text = text.to_owned();
    FauxResponseStep::Factory(Box::new(move |_context, _options, _state, model| {
        models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(model.id.clone());
        faux_assistant_message(
            FauxContent(vec![faux_text(text.clone())]),
            FauxAssistantOptions::default(),
        )
    }))
}

fn tool_call_step(name: &str) -> FauxResponseStep {
    let mut arguments = serde_json::Map::new();
    arguments.insert("path".to_owned(), json!("a.ts"));
    FauxResponseStep::Message(Box::new(faux_assistant_message(
        FauxContent(vec![faux_tool_call(name, arguments, None)]),
        FauxAssistantOptions {
            stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
            ..Default::default()
        },
    )))
}

#[tokio::test]
async fn routes_the_user_request_and_records_the_physical_response() {
    let fixture = session_fixture(false, true, InitialModel::Default, None).await;
    fixture
        .session
        .set_model(
            fixture
                .session
                .model_runtime()
                .get_model("router", "auto")
                .expect("virtual model"),
        )
        .await
        .expect("select virtual model");
    fixture
        .provider
        .append_responses(vec![assistant_reply("answer")]);

    fixture
        .session
        .prompt("hello", PromptOptions::default())
        .await
        .expect("prompt");

    // The selection stays virtual; the response records the physical model.
    assert_eq!(
        fixture.session.model().map(|m| (m.provider, m.id)),
        Some(("router".to_owned(), "auto".to_owned()))
    );
    assert_eq!(
        fixture
            .session
            .messages()
            .into_iter()
            .rev()
            .find_map(|message| match message {
                rpi_agent::AgentMessage::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .map(|assistant| (assistant.provider, assistant.model)),
        Some(("faux".to_owned(), "plan-1".to_owned()))
    );
    assert_eq!(fixture.control.reasons(), vec!["user"]);
    assert_eq!(fixture.provider.call_count(), 1);
    // The phase state entry lands on the branch.
    assert_eq!(
        fixture.state_entries(),
        vec![
            json!({"provider": "router", "modelId": "auto", "state": {"phase": "planning", "model": "plan-1"}})
        ]
    );
    // The state entry does not enter the model context.
    let contexts = fixture.ai_provider.contexts_seen();
    let contexts = contexts.lock().unwrap_or_else(|e| e.into_inner());
    let rendered = serde_json::to_string(&*contexts).expect("context json");
    assert!(
        !rendered.contains("planning"),
        "state must not enter the LLM context: {rendered}"
    );
}

#[tokio::test]
async fn continuation_after_an_edit_switches_to_the_implementation_model() {
    let fixture = session_fixture(false, true, InitialModel::Default, None).await;
    fixture
        .session
        .set_model(
            fixture
                .session
                .model_runtime()
                .get_model("router", "auto")
                .expect("virtual model"),
        )
        .await
        .expect("select virtual model");
    fixture
        .provider
        .append_responses(vec![tool_call_step("edit"), assistant_reply("done")]);

    fixture
        .session
        .prompt("refactor", PromptOptions::default())
        .await
        .expect("prompt");

    // The stub edit succeeds: the tool-result follow-up of the same turn
    // routes to the implementation model and the phase advances.
    assert_eq!(fixture.control.reasons(), vec!["user", "continuation"]);
    assert_eq!(fixture.last_assistant().model, "impl-1");
    assert_eq!(
        fixture.state_entries().last(),
        Some(
            &json!({"provider": "router", "modelId": "auto", "state": {"phase": "implementation", "model": "impl-1"}})
        )
    );
}

#[tokio::test]
async fn retry_requests_carry_the_failed_response_and_keep_the_previous() {
    let fixture = session_fixture(true, true, InitialModel::Default, None).await;
    fixture
        .session
        .set_model(
            fixture
                .session
                .model_runtime()
                .get_model("router", "auto")
                .expect("virtual model"),
        )
        .await
        .expect("select virtual model");
    fixture.provider.append_responses(vec![
        assistant_reply("first"),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "",
            FauxAssistantOptions {
                stop_reason: Some(rpi_ai::types::StopReason::Error),
                error_message: Some("529 overloaded".to_owned()),
                ..Default::default()
            },
        ))),
        assistant_reply("second"),
    ]);

    fixture
        .session
        .prompt("one", PromptOptions::default())
        .await
        .expect("first prompt");
    fixture
        .session
        .prompt("two", PromptOptions::default())
        .await
        .expect("second prompt");

    assert_eq!(fixture.control.reasons(), vec!["user", "user", "retry"]);
    assert_eq!(
        fixture.control.previous_ids(),
        vec![None, Some("plan-1".to_owned()), Some("plan-1".to_owned())]
    );
    assert_eq!(
        fixture.control.failed_present(),
        vec![false, false, true],
        "only the retry request carries the failed response"
    );
    // The retry succeeded and the final response names the physical model.
    let last = fixture.last_assistant();
    assert_eq!(last.stop_reason, rpi_ai::types::StopReason::Stop);
    assert_eq!(
        (last.provider.as_str(), last.model.as_str()),
        ("faux", "plan-1")
    );
}

#[tokio::test]
async fn a_route_failure_ends_the_run_with_an_error_response() {
    let fixture = session_fixture(false, true, InitialModel::Default, None).await;
    fixture
        .session
        .set_model(
            fixture
                .session
                .model_runtime()
                .get_model("router", "auto")
                .expect("virtual model"),
        )
        .await
        .expect("select virtual model");
    fixture
        .provider
        .append_responses(vec![assistant_reply("first")]);

    fixture
        .session
        .prompt("one", PromptOptions::default())
        .await
        .expect("first prompt");
    let provider_calls = fixture.provider.call_count();
    let route_calls = fixture.control.calls.load(Ordering::SeqCst);

    fixture.control.fail.store(true, Ordering::SeqCst);
    fixture
        .session
        .prompt("two", PromptOptions::default())
        .await
        .expect("second prompt");

    // The error response names the virtual model and carries the router's
    // message; the router ran exactly once and no provider request was made.
    let last = fixture.last_assistant();
    assert_eq!(last.stop_reason, rpi_ai::types::StopReason::Error);
    assert_eq!(
        (last.provider.as_str(), last.model.as_str()),
        ("router", "auto")
    );
    assert!(
        last.error_message
            .as_deref()
            .is_some_and(|message| message.contains("router unavailable")),
        "error message: {:?}",
        last.error_message
    );
    assert_eq!(fixture.provider.call_count(), provider_calls);
    assert_eq!(
        fixture.control.calls.load(Ordering::SeqCst),
        route_calls + 1
    );
    // The failed attempt names the virtual model; the last physical limits
    // still apply.
    assert_eq!(
        fixture
            .session
            .get_context_usage()
            .map(|usage| usage.context_window),
        Some(50_000)
    );
}

#[tokio::test]
async fn rejects_virtual_missing_and_unconfigured_route_targets() {
    let fixture = session_fixture(false, true, InitialModel::Default, None).await;
    fixture
        .session
        .set_model(
            fixture
                .session
                .model_runtime()
                .get_model("router", "auto")
                .expect("virtual model"),
        )
        .await
        .expect("select virtual model");

    for (flag, needle) in [
        (&fixture.control.route_to_virtual, "is not a physical model"),
        (&fixture.control.route_to_missing, "is not a physical model"),
        (&fixture.control.route_to_unconfigured, "has no credentials"),
    ] {
        flag.store(true, Ordering::SeqCst);
        fixture
            .session
            .prompt("hello", PromptOptions::default())
            .await
            .expect("prompt");
        let last = fixture.last_assistant();
        assert_eq!(last.stop_reason, rpi_ai::types::StopReason::Error);
        assert!(
            last.error_message
                .as_deref()
                .is_some_and(|message| message.contains(needle)),
            "expected {needle:?} in {:?}",
            last.error_message
        );
        flag.store(false, Ordering::SeqCst);
    }
    assert_eq!(fixture.provider.call_count(), 0);
}

#[tokio::test]
async fn preserves_a_supported_routed_thinking_level() {
    let fixture = session_fixture(false, true, InitialModel::Default, None).await;
    fixture
        .session
        .set_model(
            fixture
                .session
                .model_runtime()
                .get_model("router", "auto")
                .expect("virtual model"),
        )
        .await
        .expect("select virtual model");
    fixture
        .session
        .set_thinking_level(rpi_ai::types::ModelThinkingLevel::High);
    fixture
        .provider
        .append_responses(vec![assistant_reply("answer")]);

    fixture
        .session
        .prompt("hello", PromptOptions::default())
        .await
        .expect("prompt");

    // `plan-1` is a reasoning model without a level map: `high` survives.
    assert_eq!(
        fixture.last_assistant().thinking_level,
        Some(rpi_ai::types::ModelThinkingLevel::High)
    );
}

#[tokio::test]
async fn dialogue_without_a_virtual_selection_is_unchanged() {
    let fixture = session_fixture(false, true, InitialModel::Default, None).await;
    fixture
        .provider
        .append_responses(vec![assistant_reply("answer")]);

    fixture
        .session
        .prompt("hello", PromptOptions::default())
        .await
        .expect("prompt");

    // The initial model is physical (`plan-1`): no routing, no state entry.
    assert_eq!(fixture.control.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.state_entries().is_empty());
    assert_eq!(fixture.provider.call_count(), 1);
}

#[tokio::test]
async fn resume_restores_a_registered_virtual_selection() {
    let manager = restored_branch_manager(true);
    let fixture = session_fixture(false, true, InitialModel::None, Some(manager)).await;
    assert_eq!(
        fixture
            .session
            .model()
            .map(|model| (model.provider, model.id)),
        Some(("router".to_owned(), "auto".to_owned())),
        "a registered virtual selection holds"
    );
}

#[tokio::test]
async fn resume_falls_back_to_the_physical_response_without_a_registration() {
    let manager = restored_branch_manager(true);
    let fixture = session_fixture(false, false, InitialModel::None, Some(manager)).await;
    assert_eq!(
        fixture
            .session
            .model()
            .map(|model| (model.provider, model.id)),
        Some(("faux".to_owned(), "plan-1".to_owned())),
        "an unregistered virtual change falls back to the last physical response"
    );
}

/// A manager whose branch holds `model_change("router","auto")` and a
/// successful physical response from `faux/plan-1`.
fn restored_branch_manager(virtual_change: bool) -> rpi::core::session_manager::SessionManager {
    let mut manager = rpi::core::session_manager::SessionManager::in_memory(
        None,
        rpi::core::session_manager::NewSessionOptions::default(),
    )
    .expect("in-memory session");
    if virtual_change {
        manager
            .append_model_change("router", "auto")
            .expect("model change");
    }
    let mut assistant = faux_assistant_message("answer", FauxAssistantOptions::default());
    assistant.model = "plan-1".to_owned();
    manager
        .append_message(rpi_agent::AgentMessage::Assistant(assistant))
        .expect("assistant message");
    manager
}
#[tokio::test]
async fn direct_summary_requests_route_to_the_implementation_model() {
    let fixture = session_fixture(false, true, InitialModel::Default, None).await;
    fixture
        .session
        .set_model(
            fixture
                .session
                .model_runtime()
                .get_model("router", "auto")
                .expect("virtual model"),
        )
        .await
        .expect("select virtual model");
    let models_seen = Arc::new(Mutex::new(Vec::new()));
    fixture.provider.append_responses(vec![
        assistant_reply("first"),
        assistant_reply("second"),
        recording_reply(models_seen.clone(), "summary"),
        recording_reply(Arc::new(Mutex::new(Vec::new())), "turn prefix summary"),
    ]);

    fixture
        .session
        .prompt(&"a".repeat(4_000), PromptOptions::default())
        .await
        .expect("prompt");
    fixture
        .session
        .prompt(&"b".repeat(4_000), PromptOptions::default())
        .await
        .expect("prompt");
    let summary = fixture.session.compact(None).await.expect("compact");
    assert!(
        summary.summary.starts_with("summary"),
        "summary: {:?}",
        summary.summary
    );

    // The manual summary is a `direct` request; it routes to the
    // implementation model (the router ignores state for direct requests).
    assert_eq!(
        fixture.control.reasons().last().map(String::as_str),
        Some("direct")
    );
    assert_eq!(fixture.control.state_present().last(), Some(&false));
    assert_eq!(
        models_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_slice(),
        &["impl-1".to_owned()],
        "the summary request routes to the implementation model"
    );

    // The pre-compaction router state survives on the branch and comes back
    // on the next request.
    fixture.control.state_present.lock().unwrap().clear();
    fixture
        .provider
        .append_responses(vec![assistant_reply("after")]);
    fixture
        .session
        .prompt("again", PromptOptions::default())
        .await
        .expect("prompt after compact");
    assert_eq!(fixture.control.state_present().last(), Some(&true));
    assert_eq!(fixture.last_assistant().model, "plan-1");
}
