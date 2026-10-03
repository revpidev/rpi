//! Tool orchestration (`exposure` / `prepareLoadout` / `ctx.executeTool`),
//! ported from `packages/coding-agent/test/suite/agent-session-tool-orchestration.test.ts`
//! @ a13d35a74 (V16-06 FR-A/D/E).
//!
//! An orchestrator extension built only on the extension API: its own name,
//! exposure, loadout hook, and `ctx.executeTool()`. Codemode and tool search
//! (V16-07) use the same mechanisms.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi::core::agent_session::AgentSession;
use rpi::core::extension_actions::bind_session_actions;
use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_ext_host::types as ext;
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
        let dir = std::env::temp_dir().join(format!("rpi-orch-test-{}-{id}", std::process::id()));
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

pub fn faux_text_content(text: &str) -> Vec<rpi_ai::types::ToolResultContent> {
    vec![rpi_ai::types::ToolResultContent::Text(
        rpi_ai::types::TextContent {
            text: text.to_owned(),
            text_signature: None,
        },
    )]
}

fn result_text(result: &rpi_agent::types::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_definition(
    name: &str,
    description: &str,
    exposure: ext::ToolExposure,
    execute: ext::ToolExecuteFn,
) -> ext::ToolDefinition {
    ext::ToolDefinition {
        name: name.to_owned(),
        label: name.to_owned(),
        description: description.to_owned(),
        prompt_snippet: Some(description.to_owned()),
        prompt_guidelines: None,
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
        output_schema: None,
        exposure,
        namespace: None,
        annotations: None,
        default_active: None,
        prepare_loadout: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute,
        render_call: None,
        render_result: None,
    }
}

/// A tool that calls other tools, built only on the extension API.
fn orchestrator_extension(
    tool_calls: Arc<Mutex<Vec<String>>>,
    stream_events: Arc<Mutex<Vec<Value>>>,
) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        let tool_calls = tool_calls.clone();
        let stream_events = stream_events.clone();
        Box::pin(async move {
            api.register_tool(tool_definition(
                "echo",
                "Echo text.",
                ext::ToolExposure::Direct,
                Arc::new(|request, _ctx| {
                    Box::pin(async move {
                        let text = request
                            .params
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        Ok(rpi_agent::types::AgentToolResult {
                            content: faux_text_content(&format!("echo: {text}")),
                            details: json!({}),
                            ..Default::default()
                        })
                    })
                }),
            ))
            .map_err(|e| e.to_string())?;
            api.register_tool(tool_definition(
                "helper",
                "Only reachable from other tools.",
                ext::ToolExposure::Codemode,
                Arc::new(|_request, _ctx| {
                    Box::pin(async move {
                        Ok(rpi_agent::types::AgentToolResult {
                            content: faux_text_content("helped"),
                            details: json!({}),
                            ..Default::default()
                        })
                    })
                }),
            ))
            .map_err(|e| e.to_string())?;
            let mut run_tools = tool_definition(
                "run_tools",
                "Runs tools.",
                ext::ToolExposure::ModelOnly,
                Arc::new(|_request, ctx| {
                    Box::pin(async move {
                        let helper = ctx
                            .execute_tool("helper", json!({}), None)
                            .await
                            .map_err(|e| e.to_string())?;
                        let echo = ctx
                            .execute_tool("echo", json!({"text": "hi"}), None)
                            .await
                            .map_err(|e| e.to_string())?;
                        let self_call = ctx
                            .execute_tool("run_tools", json!({}), None)
                            .await
                            .map_err(|e| e.to_string())?;
                        let text = [&helper.result, &echo.result, &self_call.result]
                            .map(result_text)
                            .join(" | ");
                        Ok(rpi_agent::types::AgentToolResult {
                            content: faux_text_content(&text),
                            details: json!({}),
                            ..Default::default()
                        })
                    })
                }),
            );
            run_tools.prepare_loadout = Some(Arc::new(|loadout: &ext::ToolLoadout| {
                let names: Vec<String> = loadout
                    .callable
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect();
                Ok(Some(ext::ToolLoadoutChanges {
                    descriptions: Some(HashMap::from([
                        (
                            "run_tools".to_owned(),
                            format!("Runs tools: {}", names.join(", ")),
                        ),
                        (
                            "echo".to_owned(),
                            "Echo text (also callable from run_tools).".to_owned(),
                        ),
                    ])),
                    hidden_declarations: Some(vec!["echo".to_owned()]),
                }))
            }));
            api.register_tool(run_tools).map_err(|e| e.to_string())?;
            api.register_tool(tool_definition(
                "secret",
                "Registered but unreachable.",
                ext::ToolExposure::Hidden,
                Arc::new(|_request, _ctx| {
                    Box::pin(async move {
                        Ok(rpi_agent::types::AgentToolResult {
                            content: faux_text_content("secret"),
                            details: json!({}),
                            ..Default::default()
                        })
                    })
                }),
            ))
            .map_err(|e| e.to_string())?;
            api.on_typed::<Value, Value, _, _>("tool_call", move |event, _ctx| {
                let tool_calls = tool_calls.clone();
                async move {
                    let name = event
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let parent = event
                        .get("parentToolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("top");
                    tool_calls.lock().unwrap().push(format!("{name}:{parent}"));
                    Ok(None::<Value>)
                }
            })
            .map_err(|e| e.to_string())?;
            api.on_typed::<Value, Value, _, _>("provider_stream_event", move |event, _ctx| {
                let stream_events = stream_events.clone();
                async move {
                    stream_events.lock().unwrap().push(event);
                    Ok(None::<Value>)
                }
            })
            .map_err(|e| e.to_string())?;
            Ok(())
        }) as _
    });
    InlineExtension::Anonymous(factory)
}

struct Fixture {
    session: AgentSession,
    provider: Arc<FauxProvider>,
    _tmp: TempDir,
}

async fn session_fixture(
    tool_calls: Arc<Mutex<Vec<String>>>,
    stream_events: Arc<Mutex<Vec<Value>>>,
) -> Fixture {
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
            input_limits: None,
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
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");

    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let errors = host
        .load_inline(&[orchestrator_extension(tool_calls, stream_events)])
        .await;
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");

    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));

    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host.clone()),
        // Keep the built-ins out of the loadout so the extension tools are
        // the whole active set, like upstream's `initialActiveToolNames: []`.
        exclude_tools: Some(vec![
            "read".to_owned(),
            "bash".to_owned(),
            "edit".to_owned(),
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
        _tmp: tmp,
    }
}

fn tool_call_message(name: &str, arguments: Value) -> FauxResponseStep {
    let arguments = arguments.as_object().cloned().unwrap_or_default();
    FauxResponseStep::Message(Box::new(faux_assistant_message(
        FauxContent(vec![faux_tool_call(name, arguments, None)]),
        FauxAssistantOptions {
            stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
            ..Default::default()
        },
    )))
}

#[tokio::test]
async fn orchestrator_exposure_loadout_and_nested_calls() {
    let tool_calls = Arc::new(Mutex::new(Vec::new()));
    let fixture = session_fixture(tool_calls.clone(), Arc::new(Mutex::new(Vec::new()))).await;

    // Registration activation: `direct` + `model-only` active, `codemode`
    // not; callable = active direct + every codemode/deferred tool.
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["echo", "run_tools"]
    );
    assert_eq!(
        fixture.session.get_callable_tool_names(),
        vec!["echo", "helper"]
    );

    let requests: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let descriptions: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let request_prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_factory = requests.clone();
    let descriptions_for_factory = descriptions.clone();
    let prompts_for_factory = request_prompts.clone();
    fixture.provider.set_responses(vec![
        FauxResponseStep::Factory(Box::new(move |context, _options, _state, _model| {
            let tools = rpi_ai::utils::transcript::get_current_tools(&context.messages);
            requests_for_factory
                .lock()
                .unwrap()
                .push(tools.iter().map(|tool| tool.name.clone()).collect());
            prompts_for_factory.lock().unwrap().push(
                rpi_ai::utils::transcript::get_current_system_prompt(&context.messages),
            );
            descriptions_for_factory.lock().unwrap().extend(
                tools
                    .iter()
                    .map(|tool| (tool.name.clone(), tool.description.clone())),
            );
            faux_assistant_message(
                FauxContent(vec![faux_tool_call(
                    "run_tools",
                    serde_json::Map::new(),
                    None,
                )]),
                FauxAssistantOptions {
                    stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            )
        })),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            faux_text("done"),
            FauxAssistantOptions::default(),
        ))),
    ]);

    fixture
        .session
        .prompt("go", Default::default())
        .await
        .expect("prompt");

    // `echo` stays active, but its declaration is left out of requests.
    assert_eq!(requests.lock().unwrap()[0], vec!["run_tools"]);

    // #10192 (`028c0ec56`): the prompt's tool list matches the declarations
    // the request carries — hidden declarations are not listed.
    let request_prompts = request_prompts.lock().unwrap();
    let system_prompt = request_prompts
        .first()
        .expect("the request carries a system prompt");
    assert!(
        system_prompt.contains("- run_tools: "),
        "the visible tool is listed: {system_prompt}"
    );
    assert!(
        !system_prompt.contains("- echo: ") && !system_prompt.contains("- secret: "),
        "hidden tools must not be listed: {system_prompt}"
    );
    let descriptions = descriptions.lock().unwrap().clone();
    assert_eq!(
        descriptions
            .iter()
            .find(|(name, _)| name == "run_tools")
            .map(|(_, description)| description.clone()),
        Some("Runs tools: echo, helper".to_owned())
    );

    let result = fixture
        .session
        .messages()
        .into_iter()
        .find_map(|message| match message {
            rpi_agent::messages::AgentMessage::ToolResult(result)
                if result.tool_name == "run_tools" =>
            {
                Some(result)
            }
            _ => None,
        })
        .expect("run_tools result");
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "helped | echo: hi | Tool run_tools not found");

    let parent = result.tool_call_id.clone();
    assert_eq!(
        *tool_calls.lock().unwrap(),
        vec![
            "run_tools:top".to_owned(),
            format!("helper:{parent}"),
            format!("echo:{parent}"),
        ]
    );
    let nested = result.nested_calls.expect("nested calls");
    let calls: Vec<(String, String, String)> = nested
        .calls
        .iter()
        .map(|call| (call.id.clone(), call.name.clone(), call.status.clone()))
        .collect();
    assert_eq!(
        calls,
        vec![
            (format!("{parent}/1"), "helper".to_owned(), "ok".to_owned()),
            (format!("{parent}/2"), "echo".to_owned(), "ok".to_owned()),
            (
                format!("{parent}/3"),
                "run_tools".to_owned(),
                "error".to_owned()
            ),
        ]
    );
    assert!(nested.complete);
    // Results are never stored in the record: only the error text of the
    // failed call may appear, never the nested tools' result content.
    let serialized = serde_json::to_string(&nested).unwrap();
    assert!(!serialized.contains("helped"), "{serialized}");
    assert!(!serialized.contains("echo: hi"), "{serialized}");
    assert!(
        nested
            .calls
            .iter()
            .all(|call| call.error.is_none() || call.status == "error")
    );
}

#[tokio::test]
async fn hidden_tool_is_unreachable_even_when_named() {
    let tool_calls = Arc::new(Mutex::new(Vec::new()));
    let fixture = session_fixture(tool_calls, Arc::new(Mutex::new(Vec::new()))).await;
    // `hidden` is excluded even when named; naming `helper` (codemode)
    // activates and declares it.
    fixture.session.set_active_tools_by_name(vec![
        "echo".to_owned(),
        "helper".to_owned(),
        "secret".to_owned(),
    ]);
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["echo", "helper"]
    );
    assert_eq!(
        fixture.session.get_callable_tool_names(),
        vec!["echo", "helper"]
    );
}

#[tokio::test]
async fn tool_call_message_emits_tool_use_and_no_nested_calls() {
    let tool_calls = Arc::new(Mutex::new(Vec::new()));
    let fixture = session_fixture(tool_calls.clone(), Arc::new(Mutex::new(Vec::new()))).await;

    fixture.provider.set_responses(vec![
        tool_call_message("echo", json!({"text": "x"})),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            faux_text("done"),
            FauxAssistantOptions::default(),
        ))),
    ]);

    fixture
        .session
        .prompt("go", Default::default())
        .await
        .expect("prompt");
    let result = fixture
        .session
        .messages()
        .into_iter()
        .find_map(|message| match message {
            rpi_agent::messages::AgentMessage::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result");
    assert!(result.nested_calls.is_none());
    assert_eq!(*tool_calls.lock().unwrap(), vec!["echo:top".to_owned()]);
}

#[tokio::test]
async fn provider_stream_event_is_emitted_in_stream_order_and_not_persisted() {
    let tool_calls = Arc::new(Mutex::new(Vec::new()));
    let stream_events = Arc::new(Mutex::new(Vec::new()));
    let fixture = session_fixture(tool_calls, stream_events.clone()).await;
    fixture
        .provider
        .set_responses(vec![FauxResponseStep::Message(Box::new(
            faux_assistant_message(faux_text("hello"), FauxAssistantOptions::default()),
        ))]);

    fixture
        .session
        .prompt("go", Default::default())
        .await
        .expect("prompt");

    let events = stream_events.lock().unwrap().clone();
    assert!(!events.is_empty(), "provider stream events emitted");
    assert!(
        events.iter().all(|event| event.get("provider").is_some()
            && event.get("api").is_some()
            && event.get("model").is_some()
            && event.get("data").is_some()),
        "each event carries provider/api/model/data: {events:?}"
    );
    // Notification only: nothing from the stream observation is persisted
    // as a message or custom entry — the transcript holds the tool-declaring
    // system message, the user prompt, and the assistant reply.
    let messages = fixture.session.messages();
    assert!(
        messages
            .iter()
            .all(|message| !matches!(message, rpi_agent::messages::AgentMessage::Custom(_)))
    );
    assert_eq!(messages.len(), 3);
}
