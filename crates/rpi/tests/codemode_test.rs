//! End-to-end codemode tests (V16-07 FR-C/E/H), ported from
//! `packages/coding-agent/test/suite/agent-session-codemode.test.ts`
//! @ a13d35a74.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi::core::agent_session::AgentSession;
use rpi::core::extension_actions::bind_session_actions;
use rpi::extensions::codemode::{CodemodeMode, CodemodeSettings};
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
        let dir =
            std::env::temp_dir().join(format!("rpi-codemode-test-{}-{id}", std::process::id()));
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

fn result_text(result: &rpi_agent::types::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => Some(text.text.clone()),
            rpi_ai::types::ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_result(text: &str) -> rpi_agent::types::AgentToolResult {
    rpi_agent::types::AgentToolResult {
        content: vec![rpi_ai::types::ToolResultContent::Text(
            rpi_ai::types::TextContent {
                text: text.to_owned(),
                text_signature: None,
            },
        )],
        details: json!({}),
        ..Default::default()
    }
}

fn extension_tool(
    name: &str,
    description: &str,
    output_schema: Option<Value>,
    execute: ext::ToolExecuteFn,
) -> ext::ToolDefinition {
    ext::ToolDefinition {
        name: name.to_owned(),
        label: name.to_owned(),
        description: description.to_owned(),
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}, "additionalProperties": false}),
        constrained_sampling: None,
        output_schema,
        exposure: ext::ToolExposure::Direct,
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

/// `echo` and `stats` (the upstream fixture tools).
fn tools_extension() -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        Box::pin(async move {
            api.register_tool(extension_tool(
                "echo",
                "Echo text back.\n\nSecond paragraph.",
                None,
                Arc::new(|request, _ctx| {
                    Box::pin(async move {
                        let text = request
                            .params
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        Ok(text_result(&format!("echo: {text}")))
                    })
                }),
            ))
            .map_err(|error| error.to_string())?;
            api.register_tool(extension_tool(
                "stats",
                "Return structured stats",
                Some(json!({
                    "type": "object",
                    "properties": { "files": { "type": "number" }, "names": { "type": "array", "items": { "type": "string" } } },
                    "required": ["files", "names"],
                })),
                Arc::new(|_request, _ctx| {
                    Box::pin(async move {
                        Ok(rpi_agent::types::AgentToolResult {
                            content: vec![rpi_ai::types::ToolResultContent::Text(
                                rpi_ai::types::TextContent {
                                    text: "2 files".to_owned(),
                                    text_signature: None,
                                },
                            )],
                            details: json!({}),
                            structured_content: Some(json!({ "files": 2, "names": ["a", "b"] })),
                            ..Default::default()
                        })
                    })
                }),
            ))
            .map_err(|error| error.to_string())?;
            // Deferred tool, reachable only through `tool_search` (FR-E).
            let mut deferred = extension_tool(
                "mcp__dev__search",
                "Search the dev docs index.",
                None,
                Arc::new(|_request, _ctx| Box::pin(async move { Ok(text_result("found")) })),
            );
            deferred.exposure = ext::ToolExposure::Deferred;
            api.register_tool(deferred)
                .map_err(|error| error.to_string())?;
            Ok(())
        }) as _
    });
    InlineExtension::Anonymous(factory)
}

struct Fixture {
    session: AgentSession,
    provider: Arc<FauxProvider>,
    settings: Arc<Mutex<CodemodeSettings>>,
    _tmp: TempDir,
}

async fn session_fixture(initial_mode: CodemodeMode) -> Fixture {
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

    let settings = Arc::new(Mutex::new(CodemodeSettings {
        mode: initial_mode,
        inline_budget: None,
    }));
    let settings_fn = {
        let settings = settings.clone();
        Arc::new(move || settings.lock().unwrap().clone())
    };
    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let errors = host
        .load_inline(&[
            rpi::extensions::codemode::inline_extension(settings_fn, model_runtime.clone()),
            rpi::extensions::tool_search::inline_extension(),
            tools_extension(),
        ])
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
    created.session.set_active_tools_by_name(vec![
        "codemode".to_owned(),
        "echo".to_owned(),
        "stats".to_owned(),
    ]);

    Fixture {
        session: created.session,
        provider,
        settings,
        _tmp: tmp,
    }
}

/// Captured request tool declarations: `(name, description)` pairs.
type CapturedTools = Vec<Vec<(String, String)>>;

fn codemode_call(code: &str) -> FauxResponseStep {
    let mut arguments = serde_json::Map::new();
    arguments.insert("code".to_owned(), Value::String(code.to_owned()));
    FauxResponseStep::Message(Box::new(faux_assistant_message(
        FauxContent(vec![faux_tool_call("codemode", arguments, None)]),
        FauxAssistantOptions {
            stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
            ..Default::default()
        },
    )))
}

fn codemode_result(session: &AgentSession) -> rpi_agent::types::AgentToolResult {
    session
        .messages()
        .into_iter()
        .rev()
        .find_map(|message| match message {
            rpi_agent::messages::AgentMessage::ToolResult(result)
                if result.tool_name == "codemode" =>
            {
                Some(rpi_agent::types::AgentToolResult {
                    content: result.content,
                    details: result.details.unwrap_or(Value::Null),
                    structured_content: None,
                    usage: result.usage,
                    is_error: result.is_error.then_some(true),
                    terminate: None,
                })
            }
            _ => None,
        })
        .expect("codemode tool result")
}

async fn run(fixture: &Fixture, code: &str) -> rpi_agent::types::AgentToolResult {
    fixture.provider.set_responses(vec![
        codemode_call(code),
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
    codemode_result(&fixture.session)
}

#[tokio::test]
async fn runs_nested_calls_in_parallel_and_returns_only_the_script_result() {
    let fixture = session_fixture(CodemodeMode::On).await;
    let result = run(
        &fixture,
        r#"
			const [a, b, stats] = await Promise.all([
				tools.echo({ text: "one" }),
				tools.echo({ text: "two" }),
				tools.stats({}),
			]);
			console.log("files", stats.files);
			text(ALL_TOOLS.map((tool) => tool.name).join(","));
			return { a, b, names: stats.names };
		"#,
    )
    .await;

    assert_eq!(result.is_error, None);
    let text = result_text(&result);
    let (header, rest) = text.split_once("Output:\n").expect("header");
    assert!(
        header.starts_with("Script completed\nWall time "),
        "{header}"
    );
    assert_eq!(
        rest.trim_start_matches('\n'),
        "files 2\necho,stats,mcp__dev__search\n{\"a\":\"echo: one\",\"b\":\"echo: two\",\"names\":[\"a\",\"b\"]}"
    );
    let calls = result.details["calls"].as_array().expect("calls");
    let statuses: Vec<(&str, &str)> = calls
        .iter()
        .map(|call| {
            (
                call["name"].as_str().unwrap_or_default(),
                call["status"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        vec![("echo", "ok"), ("echo", "ok"), ("stats", "ok")]
    );
    // Nested calls never become transcript tool results.
    let tool_results = fixture
        .session
        .messages()
        .into_iter()
        .filter(|message| matches!(message, rpi_agent::messages::AgentMessage::ToolResult(_)))
        .count();
    assert_eq!(tool_results, 1);
}

#[tokio::test]
async fn presents_callable_tools_per_codemode_mode() {
    let fixture = session_fixture(CodemodeMode::On).await;
    let requests: Arc<Mutex<CapturedTools>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_first = requests.clone();
    fixture.provider.set_responses(vec![
        FauxResponseStep::Factory(Box::new(move |context, _options, _state, _model| {
            let tools = rpi_ai::utils::transcript::get_current_tools(&context.messages);
            requests_for_first.lock().unwrap().push(
                tools
                    .iter()
                    .map(|tool| (tool.name.clone(), tool.description.clone()))
                    .collect(),
            );
            faux_assistant_message(faux_text("ok"), FauxAssistantOptions::default())
        })),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            faux_text("done"),
            FauxAssistantOptions::default(),
        ))),
    ]);
    fixture
        .session
        .prompt("on", Default::default())
        .await
        .expect("prompt on");
    let on_tools = requests.lock().unwrap()[0].clone();
    let description = |name: &str| {
        on_tools
            .iter()
            .find(|(tool, _)| tool == name)
            .map(|(_, description)| description.clone())
            .unwrap_or_default()
    };
    let echo = description("echo");
    assert!(
        echo.contains("Codemode: `tools.echo(args)` resolves to a string."),
        "{echo}"
    );
    assert!(!echo.contains("codemode tool declaration:"), "{echo}");
    let codemode = description("codemode");
    assert!(!codemode.contains("### `echo`"), "{codemode}");

    // only: codemode lists echo, which stays active but is left out of requests.
    fixture.settings.lock().unwrap().mode = CodemodeMode::Only;
    fixture.session.set_active_tools_by_name(vec![
        "codemode".to_owned(),
        "echo".to_owned(),
        "stats".to_owned(),
    ]);
    let requests_for_second = requests.clone();
    fixture.provider.set_responses(vec![
        FauxResponseStep::Factory(Box::new(move |context, _options, _state, _model| {
            let tools = rpi_ai::utils::transcript::get_current_tools(&context.messages);
            requests_for_second.lock().unwrap().push(
                tools
                    .iter()
                    .map(|tool| (tool.name.clone(), tool.description.clone()))
                    .collect(),
            );
            faux_assistant_message(faux_text("ok"), FauxAssistantOptions::default())
        })),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            faux_text("done"),
            FauxAssistantOptions::default(),
        ))),
    ]);
    fixture
        .session
        .prompt("only", Default::default())
        .await
        .expect("prompt only");
    let only_tools = requests.lock().unwrap()[1].clone();
    let names: Vec<&str> = only_tools.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"codemode"), "{names:?}");
    assert!(!names.contains(&"echo"), "hidden declarations: {names:?}");
    let codemode = only_tools
        .iter()
        .find(|(name, _)| name == "codemode")
        .map(|(_, description)| description.clone())
        .expect("codemode description");
    assert!(codemode.contains("### `echo`"), "{codemode}");
}

#[tokio::test]
async fn keeps_store_values_across_calls() {
    let fixture = session_fixture(CodemodeMode::On).await;
    let first = run(
        &fixture,
        "const next = (load(\"count\") ?? 0) + 1;\nstore(\"count\", next);\nreturn next;",
    )
    .await;
    assert!(
        result_text(&first).trim_end().ends_with('1'),
        "{}",
        result_text(&first)
    );
    let second = run(
        &fixture,
        "const next = (load(\"count\") ?? 0) + 1;\nstore(\"count\", next);\nreturn next;",
    )
    .await;
    assert!(
        result_text(&second).trim_end().ends_with('2'),
        "{}",
        result_text(&second)
    );

    let entries = fixture
        .session
        .session_manager()
        .lock()
        .unwrap()
        .get_branch(None);
    assert!(
        entries
            .iter()
            .any(|entry| entry.raw_value().to_string().contains("codemode-store")),
        "expected codemode-store entries"
    );
}

#[tokio::test]
async fn applies_the_timeout_option_and_rejects_invalid_options() {
    let fixture = session_fixture(CodemodeMode::On).await;
    let timed_out = run(
        &fixture,
        "// @options: {\"timeout_ms\": 200}\nwhile (true) {}",
    )
    .await;
    assert_eq!(timed_out.is_error, Some(true));
    assert!(
        result_text(&timed_out).contains("Script error:\nScript timed out"),
        "{}",
        result_text(&timed_out)
    );

    let invalid = run(&fixture, "// @options: {\"yield\": 1}\ntext(1)").await;
    assert_eq!(invalid.is_error, Some(true));
    assert_eq!(
        result_text(&invalid),
        "@options only supports `max_output_tokens` and `timeout_ms`; got `yield`"
    );
}

#[tokio::test]
async fn reports_script_failures_with_partial_output_and_calls() {
    let fixture = session_fixture(CodemodeMode::On).await;
    let result = run(
        &fixture,
        "text(\"partial\");\nawait tools.echo({ text: \"x\" });\nthrow new Error(\"boom\");",
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let text = result_text(&result);
    assert!(text.starts_with("Script failed\n"), "{text}");
    assert!(
        text.contains("partial\nScript error:\nError: boom"),
        "{text}"
    );
    assert!(text.contains("codemode.js:3"), "{text}");
    assert!(
        text.contains("Tool calls made before the failure (they are not undone): echo (ok)"),
        "{text}"
    );
}
#[tokio::test]
async fn tool_search_loads_deferred_tools_and_keeps_codemode_description_stable() {
    let fixture = session_fixture(CodemodeMode::On).await;
    fixture.session.set_active_tools_by_name(vec![
        "codemode".to_owned(),
        "echo".to_owned(),
        "stats".to_owned(),
        "tool_search".to_owned(),
    ]);
    let codemode_description_before = fixture
        .session
        .agent()
        .state()
        .tools
        .iter()
        .find(|tool| tool.name() == "codemode")
        .map(|tool| tool.description().to_owned())
        .expect("codemode tool");

    let arguments = json!({ "query": "dev docs search" })
        .as_object()
        .cloned()
        .expect("arguments");
    fixture.provider.set_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            FauxContent(vec![faux_tool_call("tool_search", arguments, None)]),
            FauxAssistantOptions {
                stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
                ..Default::default()
            },
        ))),
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
        .rev()
        .find_map(|message| match message {
            rpi_agent::messages::AgentMessage::ToolResult(result)
                if result.tool_name == "tool_search" =>
            {
                Some(result)
            }
            _ => None,
        })
        .expect("tool_search result");
    assert!(!result.is_error);
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        text,
        "Loaded 1 tool. They are available from your next call:\n- mcp__dev__search: Search the dev docs index."
    );
    assert_eq!(
        result.details.unwrap_or(Value::Null)["loaded"],
        json!(["mcp__dev__search"])
    );

    // Activation: the deferred tool joins the active set (declared next call).
    assert!(
        fixture
            .session
            .get_active_tool_names()
            .contains(&"mcp__dev__search".to_owned()),
        "{:?}",
        fixture.session.get_active_tool_names()
    );
    // #10212: loading a tool must not change the codemode description.
    let codemode_description_after = fixture
        .session
        .agent()
        .state()
        .tools
        .iter()
        .find(|tool| tool.name() == "codemode")
        .map(|tool| tool.description().to_owned())
        .expect("codemode tool");
    assert_eq!(codemode_description_before, codemode_description_after);
}
