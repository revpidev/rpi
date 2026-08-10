//! T15 W2 event-gap wiring tests: every case drives the REAL host through
//! the full chain "native inline factory registers handlers →
//! NativeExtensionHost → ExtensionHostAdapter → ExtensionRunner seam"; no
//! traits are mocked.
//!
//! Upstream anchors: `agent-session.ts` (_installAgentToolHooks :459-517,
//! compact :1783-1925, extendResourcesFromExtensions :2254-2277,
//! before_agent_start :1224-1253), `interactive-mode.ts:5931-5940`
//! (user_bash), `resource-loader.ts:327-335,520-571` (two-phase load),
//! `project-trust.ts:54-70` (project_trust resolution).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rpi::core::extension_host_adapter::ExtensionHostAdapter;
use rpi::core::extensions::{
    extension_on_response_callback, new_extension_runner_ref, ExtensionRunner,
};
use rpi_agent::messages::AgentMessage;
use rpi_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_ext_host::types as ext;
use rpi_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Shared fixture helpers
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rpi-ext-w2-test-{}-{id}", std::process::id()));
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

/// An inline native extension whose factory runs `register` against the API.
fn inline_ext(
    register: impl Fn(&rpi_ext_host::api::ExtensionApi) + Send + Sync + 'static,
) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api| {
        register(&api);
        Box::pin(async { Ok(()) })
    });
    InlineExtension::Anonymous(factory)
}

/// JSON event handler shorthand.
fn on_json(
    api: &rpi_ext_host::api::ExtensionApi,
    event: &str,
    f: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
) {
    api.on(
        event,
        Arc::new(move |payload, _ctx| {
            let result = f(payload);
            Box::pin(async move { result })
        }),
    )
    .expect("register handler");
}

async fn host_with(extensions: Vec<InlineExtension>) -> NativeExtensionHost {
    let host = NativeExtensionHost::new("/w2-cwd");
    let errors = host.load_inline(&extensions).await;
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    host
}

async fn runner_with(extensions: Vec<InlineExtension>) -> Arc<dyn ExtensionRunner> {
    let host = host_with(extensions).await;
    Arc::new(ExtensionHostAdapter::new(Arc::new(host)))
}

// ---------------------------------------------------------------------------
// Recording tool + full session fixture (pattern: agent_session_test.rs)
// ---------------------------------------------------------------------------

struct RecorderTool {
    parameters: Value,
    calls: Arc<Mutex<Vec<Value>>>,
}

#[async_trait::async_trait]
impl AgentTool for RecorderTool {
    fn name(&self) -> &str {
        "recorder"
    }
    fn label(&self) -> &str {
        "recorder"
    }
    fn description(&self) -> &str {
        "records call arguments"
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(params.clone());
        Ok(AgentToolResult {
            content: vec![rpi_ai::types::ToolResultContent::Text(
                rpi_ai::types::TextContent {
                    text: format!("executed with {params}"),
                    text_signature: None,
                },
            )],
            ..Default::default()
        })
    }
}

fn recorder_call_step(args: Value) -> FauxResponseStep {
    faux_assistant_message(
        faux_tool_call(
            "recorder",
            args.as_object().cloned().unwrap_or_default(),
            None,
        ),
        FauxAssistantOptions::default(),
    )
    .into()
}

fn text_step(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

struct SessionFixture {
    session: rpi::core::agent_session::AgentSession,
    services: rpi::core::agent_session_services::AgentSessionServices,
    _tmp: TempDir,
}

/// Full pipeline: `create_agent_session` with the real extension host
/// (the real path where sdk.rs hooks in) + faux provider scripted responses.
async fn session_fixture(
    responses: Vec<FauxResponseStep>,
    host: Arc<NativeExtensionHost>,
    custom_tools: Vec<Arc<dyn AgentTool>>,
) -> SessionFixture {
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

    let active_tools: Vec<String> = custom_tools
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        tools: Some(active_tools),
        custom_tools,
        services: Some(services.clone()),
        session_manager: Some(session_manager),
        extension_host: Some(host),
        ..Default::default()
    })
    .await
    .expect("create session");

    SessionFixture {
        session: created.session,
        services,
        _tmp: tmp,
    }
}

/// JSON view of the first toolResult in the session messages.
fn tool_result_json(session: &rpi::core::agent_session::AgentSession) -> Value {
    session
        .messages()
        .into_iter()
        .map(|message| serde_json::to_value(&message).expect("message json"))
        .find(|value| value.get("role").and_then(Value::as_str) == Some("toolResult"))
        .expect("toolResult message")
}

// ---------------------------------------------------------------------------
// tool_call（agent-session.ts:466-485 + runner.ts:919-940）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w2_tool_call_block_short_circuits_execution() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let host = host_with(vec![inline_ext(|api| {
        on_json(api, ext::EVENT_TOOL_CALL, |_| {
            Ok(json!({"block": true, "reason": "policy denies recorder"}))
        });
    })])
    .await;
    let ext_errors = Arc::new(Mutex::new(Vec::new()));
    let sink = ext_errors.clone();
    let _unsub = host.on_error(Arc::new(move |e| {
        sink.lock().unwrap_or_else(|e2| e2.into_inner()).push(e);
    }));

    let fixture = session_fixture(
        vec![recorder_call_step(json!({"a": 1})), text_step("done")],
        Arc::new(host),
        vec![Arc::new(RecorderTool {
            parameters: json!({"type": "object"}),
            calls: calls.clone(),
        })],
    )
    .await;

    fixture
        .session
        .prompt("run it", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    // block: the tool does not execute; the error tool result carries a reason.
    assert!(calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    let result = tool_result_json(&fixture.session);
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("policy denies recorder"), "content: {text}");
    // No handler error → no extension error.
    assert!(ext_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w2_tool_call_args_mutation_applies_without_revalidation() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let host = host_with(vec![inline_ext(|api| {
        // Upstream mutates event.input in place; rpi threads it through the result's
        // "input" field (see the runner.rs emit_tool_call divergence note).
        on_json(api, ext::EVENT_TOOL_CALL, |event| {
            let mut input = event["input"].clone();
            input["patched"] = json!(true);
            Ok(json!({"input": input}))
        });
    })])
    .await;

    let fixture = session_fixture(
        vec![recorder_call_step(json!({"a": 1})), text_step("done")],
        Arc::new(host),
        vec![Arc::new(RecorderTool {
            parameters: json!({"type": "object"}),
            calls: calls.clone(),
        })],
    )
    .await;

    fixture
        .session
        .prompt("run it", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], json!({"a": 1, "patched": true}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w2_tool_call_handler_error_fail_safe_blocks() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let host = host_with(vec![inline_ext(|api| {
        on_json(api, ext::EVENT_TOOL_CALL, |_| {
            Err("handler exploded".to_owned())
        });
    })])
    .await;
    let ext_errors = Arc::new(Mutex::new(Vec::new()));
    let sink = ext_errors.clone();
    let _unsub = host.on_error(Arc::new(move |e| {
        sink.lock().unwrap_or_else(|e2| e2.into_inner()).push(e);
    }));

    let fixture = session_fixture(
        vec![recorder_call_step(json!({"a": 1})), text_step("done")],
        Arc::new(host),
        vec![Arc::new(RecorderTool {
            parameters: json!({"type": "object"}),
            calls: calls.clone(),
        })],
    )
    .await;

    fixture
        .session
        .prompt("run it", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    // fail-safe: the tool is blocked; the reason prefix aligns with upstream
    // "Extension failed, blocking execution"（agent-session.ts:482）。
    assert!(calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    let result = tool_result_json(&fixture.session);
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Extension failed, blocking execution: handler exploded"),
        "content: {text}"
    );
    // The error also enters the extension error bus.
    let errors = ext_errors.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].event, "tool_call");
    assert_eq!(errors[0].error, "handler exploded");
}

// ---------------------------------------------------------------------------
// tool_result（agent-session.ts:487-517 + runner.ts:864-917）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w2_tool_result_partial_patches_chain_across_extensions() {
    let observed = Arc::new(Mutex::new(String::new()));
    let seen = observed.clone();
    let host = host_with(vec![
        inline_ext(|api| {
            on_json(api, ext::EVENT_TOOL_RESULT, |_| {
                Ok(json!({"content": [{"type": "text", "text": "patched-by-a"}]}))
            });
        }),
        inline_ext(move |api| {
            let seen = seen.clone();
            on_json(api, ext::EVENT_TOOL_RESULT, move |event| {
                // Chained: sees ext-a's patch (runner.ts:878-893).
                *seen.lock().unwrap_or_else(|e| e.into_inner()) = event["content"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                Ok(json!({"isError": true}))
            });
        }),
    ])
    .await;

    let fixture = session_fixture(
        vec![recorder_call_step(json!({"a": 1})), text_step("done")],
        Arc::new(host),
        vec![Arc::new(RecorderTool {
            parameters: json!({"type": "object"}),
            calls: Arc::new(Mutex::new(Vec::new())),
        })],
    )
    .await;

    fixture
        .session
        .prompt("run it", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    assert_eq!(
        observed.lock().unwrap_or_else(|e| e.into_inner()).as_str(),
        "patched-by-a"
    );
    let result = tool_result_json(&fixture.session);
    assert_eq!(result["content"][0]["text"], "patched-by-a");
    assert_eq!(result["isError"], true);
}

// ---------------------------------------------------------------------------
// session_before_compact / session_compact
// (agent-session.ts:1783-1925 manual; compaction_runner_test.rs fixture pattern)
// ---------------------------------------------------------------------------

struct CompactionFixture {
    agent: Arc<rpi_agent::Agent>,
    runner: rpi::core::compaction_runner::CompactionRunner,
    events: Arc<Mutex<Vec<rpi::core::compaction_runner::CompactionEvent>>>,
}

fn compaction_fixture(
    responses: Vec<FauxResponseStep>,
    runner_ref: Option<rpi::core::extensions::ExtensionRunnerRef>,
) -> CompactionFixture {
    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(8192),
            max_tokens: Some(65536),
        }]),
        ..Default::default()
    });
    provider.set_responses(responses);
    let model = provider.get_model(None).expect("faux-1");

    let mut options = rpi_agent::AgentOptions::new(provider.stream_fn());
    options.initial_state = rpi_agent::InitialAgentState {
        model: Some(model.clone()),
        thinking_level: Some(rpi_agent::types::ThinkingLevel::Off),
        ..Default::default()
    };
    let agent = Arc::new(rpi_agent::Agent::new(options));

    let session = rpi::core::session_manager::SessionManager::in_memory(
        None,
        rpi::core::session_manager::NewSessionOptions::default(),
    )
    .expect("session");
    let events: Arc<Mutex<Vec<rpi::core::compaction_runner::CompactionEvent>>> =
        Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let mut runner = rpi::core::compaction_runner::CompactionRunner::new(
        agent.clone(),
        Arc::new(Mutex::new(session)),
        Some(model),
        rpi_agent::compaction::CompactionSettings {
            enabled: true,
            reserve_tokens: 4096,
            keep_recent_tokens: 10,
        },
        None,
        provider.stream_fn(),
        rpi_agent::types::ThinkingLevel::Off,
        Arc::new(move |event| sink.lock().expect("events").push(event)),
    );
    if let Some(runner_ref) = runner_ref {
        runner.set_extension_runner(runner_ref);
    }
    CompactionFixture {
        agent,
        runner,
        events,
    }
}

impl CompactionFixture {
    fn seed_turn(&mut self, user_text: &str, assistant_text: &str, total_tokens: u64) {
        let user: AgentMessage = serde_json::from_value(json!({
            "role": "user", "content": user_text, "timestamp": 1,
        }))
        .expect("user");
        let mut assistant = faux_assistant_message(assistant_text, FauxAssistantOptions::default());
        assistant.usage = serde_json::from_value(json!({
            "input": total_tokens, "output": 0, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": total_tokens,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
        }))
        .expect("usage");
        let mut messages = Vec::new();
        self.runner
            .session_mut()
            .append_message(user.clone())
            .expect("append user");
        self.runner
            .session_mut()
            .append_message(AgentMessage::Assistant(assistant.clone()))
            .expect("append assistant");
        messages.push(user);
        messages.push(AgentMessage::Assistant(assistant));
        self.agent.set_messages(messages);
    }

    fn seed_three_turns(&mut self) {
        self.seed_turn(
            &format!("q1 {}", "x".repeat(80)),
            &format!("a1 {}", "y".repeat(80)),
            100,
        );
        self.seed_turn(&format!("q2 {}", "x".repeat(80)), "a2", 100);
        self.seed_turn("q3", "a3", 100);
    }
}

#[tokio::test]
async fn w2_session_before_compact_cancel_aborts_manual_compaction() {
    // agent-session.ts:1819-1821：cancel → "Compaction cancelled" →
    // compaction_end{aborted:true, errorMessage:undefined}。
    let host = host_with(vec![inline_ext(|api| {
        on_json(api, ext::EVENT_SESSION_BEFORE_COMPACT, |event| {
            // Payload-completeness check: preparation/branchEntries/reason/willRetry.
            assert!(event["preparation"].is_object());
            assert!(event["branchEntries"]
                .as_array()
                .is_some_and(|e| !e.is_empty()));
            assert_eq!(event["reason"], "manual");
            assert_eq!(event["willRetry"], false);
            Ok(json!({"cancel": true}))
        });
    })])
    .await;
    let runner_ref = new_extension_runner_ref(
        Arc::new(ExtensionHostAdapter::new(Arc::new(host))) as Arc<dyn ExtensionRunner>
    );

    let mut fixture = compaction_fixture(Vec::new(), Some(runner_ref));
    fixture.seed_three_turns();

    let error = fixture.runner.compact(None).await.expect_err("cancelled");
    assert_eq!(error.to_string(), "session error: Compaction cancelled");
    let events = fixture.events.lock().expect("events");
    let end = events
        .iter()
        .find_map(|event| match event {
            rpi::core::compaction_runner::CompactionEvent::CompactionEnd {
                aborted,
                error_message,
                ..
            } => Some((*aborted, error_message.clone())),
            _ => None,
        })
        .expect("compaction_end");
    assert_eq!(end, (true, None));
}

#[tokio::test]
async fn w2_session_before_compact_replacement_sets_from_extension_and_emits_session_compact() {
    // agent-session.ts:1823-1827 + :1872-1891: when the extension provides a complete
    // CompactionResult, skip the default summary, appendCompaction(fromExtension=true),
    // and emit session_compact.
    let captured = Arc::new(Mutex::new(Value::Null));
    let sink = captured.clone();
    let host = host_with(vec![inline_ext(move |api| {
        on_json(api, ext::EVENT_SESSION_BEFORE_COMPACT, |event| {
            let entries = event["branchEntries"].as_array().expect("entries");
            let last_id = entries.last().expect("last")["id"]
                .as_str()
                .expect("id")
                .to_owned();
            Ok(json!({
                "compaction": {
                    "summary": "ext-summary",
                    "firstKeptEntryId": last_id,
                    "tokensBefore": 42,
                }
            }))
        });
        let sink = sink.clone();
        on_json(api, ext::EVENT_SESSION_COMPACT, move |event| {
            *sink.lock().unwrap_or_else(|e| e.into_inner()) = event;
            Ok(Value::Null)
        });
    })])
    .await;
    let runner_ref = new_extension_runner_ref(
        Arc::new(ExtensionHostAdapter::new(Arc::new(host))) as Arc<dyn ExtensionRunner>
    );

    let mut fixture = compaction_fixture(Vec::new(), Some(runner_ref));
    fixture.seed_three_turns();

    let result = fixture.runner.compact(None).await.expect("compact");
    assert_eq!(result.summary, "ext-summary");
    assert_eq!(result.tokens_before, 42);
    assert!(result.estimated_tokens_after.is_some());

    let event = captured.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(event["type"], "session_compact");
    assert_eq!(event["fromExtension"], true);
    assert_eq!(event["reason"], "manual");
    assert_eq!(event["willRetry"], false);
    assert_eq!(event["compactionEntry"]["type"], "compaction");
    assert_eq!(event["compactionEntry"]["summary"], "ext-summary");
    // The persisted field on the entry is called `fromHook` upstream
    // (session-manager.ts:79).
    assert_eq!(event["compactionEntry"]["fromHook"], true);
}

#[tokio::test]
async fn w2_session_before_compact_no_handlers_runs_default_path() {
    // No handler: default summary path (faux provides the summary text),
    // fromExtension=false.
    let runner_ref = new_extension_runner_ref(runner_with(Vec::new()).await);
    let mut fixture = compaction_fixture(
        vec![FauxResponseStep::Factory(Box::new(
            |_ctx, _opts, _state, _model| {
                faux_assistant_message("DEFAULT SUMMARY", FauxAssistantOptions::default())
            },
        ))],
        Some(runner_ref),
    );
    fixture.seed_three_turns();
    let result = fixture.runner.compact(None).await.expect("compact");
    assert!(result.summary.starts_with("DEFAULT SUMMARY"));
}

// ---------------------------------------------------------------------------
// after_provider_response (sdk.rs stream_fn on_response wiring)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w2_after_provider_response_reaches_extension() {
    let captured = Arc::new(Mutex::new(Value::Null));
    let sink = captured.clone();
    let runner = runner_with(vec![inline_ext(move |api| {
        let sink = sink.clone();
        on_json(api, ext::EVENT_AFTER_PROVIDER_RESPONSE, move |event| {
            *sink.lock().unwrap_or_else(|e| e.into_inner()) = event;
            Ok(Value::Null)
        });
    })])
    .await;

    let callback = extension_on_response_callback(new_extension_runner_ref(runner));
    let provider = FauxProvider::new(FauxProviderOptions::default());
    let model = provider.get_model(None).expect("model");
    let mut headers = std::collections::HashMap::new();
    headers.insert("x-request-id".to_owned(), "r-1".to_owned());
    callback(
        rpi_ai::types::ProviderResponse {
            status: 200,
            headers,
        },
        &model,
    )
    .await;

    let event = captured.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(event["type"], "after_provider_response");
    assert_eq!(event["status"], 200);
    assert_eq!(event["headers"]["x-request-id"], "r-1");
}

#[tokio::test]
async fn w2_after_provider_response_skipped_without_handlers() {
    // has_handlers gate (same short-circuit as agent-session.ts:467-470).
    let hit = Arc::new(AtomicUsize::new(0));
    let marker = hit.clone();
    let runner = runner_with(vec![inline_ext(move |api| {
        let marker = marker.clone();
        on_json(api, ext::EVENT_SESSION_START, move |_| {
            marker.fetch_add(1, Ordering::Relaxed);
            Ok(Value::Null)
        });
    })])
    .await;
    let callback = extension_on_response_callback(new_extension_runner_ref(runner));
    let provider = FauxProvider::new(FauxProviderOptions::default());
    let model = provider.get_model(None).expect("model");
    callback(
        rpi_ai::types::ProviderResponse {
            status: 500,
            headers: std::collections::HashMap::new(),
        },
        &model,
    )
    .await;
    assert_eq!(hit.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// user_bash（interactive-mode.ts:5931-5940）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w2_user_bash_full_result_replacement() {
    let runner = runner_with(vec![inline_ext(|api| {
        on_json(api, ext::EVENT_USER_BASH, |event| {
            assert_eq!(event["command"], "ls -la");
            assert_eq!(event["excludeFromContext"], true);
            Ok(json!({
                "result": {
                    "output": "extension output",
                    "exitCode": 0,
                    "cancelled": false,
                    "truncated": false,
                }
            }))
        });
    })])
    .await;

    let result = runner
        .emit_user_bash("ls -la", true, "/w2-cwd")
        .await
        .expect("replacement");
    assert_eq!(result.output, "extension output");
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.cancelled);
    assert!(!result.truncated);
}

#[tokio::test]
async fn w2_user_bash_operations_only_and_no_handler_fall_back() {
    // operations (closure bundle) cannot cross the JSON boundary → dropped fallback
    // (candidate divergence); no handler → None (the caller runs default bash
    // execution).
    let runner = runner_with(vec![inline_ext(|api| {
        on_json(api, ext::EVENT_USER_BASH, |_| {
            Ok(json!({"operations": {"note": "custom backend"}}))
        });
    })])
    .await;
    assert!(runner.emit_user_bash("x", false, "/w2-cwd").await.is_none());

    let empty = runner_with(Vec::new()).await;
    assert!(empty.emit_user_bash("x", false, "/w2-cwd").await.is_none());
}

// ---------------------------------------------------------------------------
// project_trust two-phase loading (resource-loader.ts:327-335,520-571 +
// project-trust.ts:54-70）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w2_project_trust_two_phase_load_and_extension_decision() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    let agent_dir = tmp.path().join("agent");
    // Project-local extensions (trust-gated objects) + global extensions.
    std::fs::create_dir_all(cwd.join(".rpi/extensions")).expect("project ext dir");
    std::fs::write(cwd.join(".rpi/extensions/local.wasm"), "wasm").expect("local.wasm");
    std::fs::create_dir_all(agent_dir.join("extensions")).expect("global ext dir");
    std::fs::write(agent_dir.join("extensions/global.wasm"), "wasm").expect("global.wasm");

    let factory_runs = Arc::new(AtomicUsize::new(0));
    let runs = factory_runs.clone();
    let trust_ext = inline_ext(move |api| {
        runs.fetch_add(1, Ordering::Relaxed);
        on_json(api, ext::EVENT_PROJECT_TRUST, |_| {
            Ok(json!({"trusted": "no", "remember": false}))
        });
    });

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    // Phase one (pre-trust): global + CLI + inline; project-local excluded.
    let pre_errors = host
        .load_startup_pre_trust(agent_dir.clone(), Vec::new(), vec![trust_ext], false)
        .await;
    assert_eq!(factory_runs.load(Ordering::Relaxed), 1);
    assert!(
        pre_errors.iter().any(|e| e.path.contains("global.wasm")),
        "global attempted pre-trust: {pre_errors:?}"
    );
    assert!(
        !pre_errors.iter().any(|e| e.path.contains("local.wasm")),
        "project-local must stay out pre-trust: {pre_errors:?}"
    );

    // project_trust resolution (the extension_event priority slot of
    // project-trust.ts:54-70).
    let (result, errors) = host
        .emit_project_trust(json!({"type": "project_trust", "cwd": cwd.to_string_lossy()}))
        .await;
    assert!(errors.is_empty());
    let event = result
        .as_ref()
        .map(rpi::core::extension_host_adapter::parse_project_trust_result)
        .expect("decision");
    let trust_store = rpi::core::trust_manager::ProjectTrustStore::new(&agent_dir);
    let mut context = rpi::core::trust_manager::ProjectTrustContext::headless();
    let trusted = rpi::core::trust_manager::resolve_project_trusted(
        &cwd,
        &trust_store,
        None,
        rpi::core::trust_manager::DefaultProjectTrust::Ask,
        Some(event),
        &mut context,
    )
    .expect("resolve");
    // Extension says no → not trusted (the early-exit branch would give true, so this
    // assertion proves the event was consumed).
    assert!(!trusted);

    // Phase two (final): full load; pre-trust inline extensions are reused (factory
    // not re-run); project-local paths only enter the attempt set now.
    let final_errors = host
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            false,
        )
        .await;
    assert_eq!(
        factory_runs.load(Ordering::Relaxed),
        1,
        "pre-trust inline factory must be reused, not re-run"
    );
    assert!(
        final_errors.iter().any(|e| e.path.contains("local.wasm")),
        "project-local attempted post-trust: {final_errors:?}"
    );
    assert!(
        final_errors.iter().any(|e| e.path.contains("global.wasm")),
        "pre-trust errors carried over: {final_errors:?}"
    );
    // Inline extensions are in the final extension set (at the tail).
    assert_eq!(host.get_extension_paths(), ["<inline:1>"]);
}

// ---------------------------------------------------------------------------
// resources_discover（agent-session.ts:2254-2277）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w2_resources_discover_extends_resource_loader() {
    let tmp = TempDir::new();
    let skill_dir = tmp.path().join("ext-skills/demo-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: from extension\n---\n\nBody.\n",
    )
    .expect("SKILL.md");
    let skill_file = skill_dir.join("SKILL.md").to_string_lossy().into_owned();

    let host = host_with(vec![inline_ext(move |api| {
        let skill_file = skill_file.clone();
        on_json(api, ext::EVENT_RESOURCES_DISCOVER, move |event| {
            assert_eq!(event["reason"], "startup");
            Ok(json!({"skillPaths": [skill_file]}))
        });
    })])
    .await;

    let fixture = session_fixture(vec![text_step("hi")], Arc::new(host), Vec::new()).await;

    // Fires after session_start inside bind_extensions (agent-session.ts:2249-2251).
    fixture
        .session
        .bind_extensions(rpi::core::agent_session::ExtensionBindings {
            mode: None,
            on_error: None,
            shutdown: None,
        })
        .await;

    let loader = fixture
        .services
        .resource_loader
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let skills = &loader.resources().skills;
    let skill = skills
        .iter()
        .find(|skill| skill.name == "demo-skill")
        .expect("extension skill loaded");
    assert!(
        skill.source_info.source.starts_with("extension:"),
        "source label: {}",
        skill.source_info.source
    );
}

// ---------------------------------------------------------------------------
// before_agent_start chaining (agent-session.ts:1224-1253 + runner.ts:1068-1132)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w2_before_agent_start_chains_system_prompt_and_injects_messages() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let log = observed.clone();
    let host = host_with(vec![
        inline_ext(|api| {
            on_json(api, ext::EVENT_BEFORE_AGENT_START, |event| {
                // The real base system prompt is passed in by the caller (non-empty).
                assert!(event["systemPrompt"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()));
                Ok(json!({
                    "message": {"customType": "injected-note", "display": false},
                    "systemPrompt": "prompt-A",
                }))
            });
        }),
        inline_ext(move |api| {
            let log = log.clone();
            on_json(api, ext::EVENT_BEFORE_AGENT_START, move |event| {
                log.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event["systemPrompt"].as_str().unwrap_or("").to_owned());
                Ok(json!({"systemPrompt": "prompt-B"}))
            });
        }),
    ])
    .await;

    let fixture = session_fixture(vec![text_step("done")], Arc::new(host), Vec::new()).await;
    fixture
        .session
        .prompt("hello", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    // Chained: ext-b sees ext-a's replaced prompt; the effective prompt is prompt-B.
    assert_eq!(
        observed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_slice(),
        ["prompt-A"]
    );
    assert_eq!(fixture.session.system_prompt(), "prompt-B");
    // The injected custom message is queued (role:"custom").
    let has_injected = fixture.session.messages().into_iter().any(|message| {
        let value = serde_json::to_value(&message).expect("json");
        value.get("role").and_then(Value::as_str) == Some("custom")
            && value.get("customType").and_then(Value::as_str) == Some("injected-note")
    });
    assert!(has_injected, "injected custom message present");
}
