//! Tool selection governance tests (V17-04 FR-A/FR-C).
//!
//! Port of `packages/coding-agent/test/suite/agent-session-mcp.test.ts`
//! (`keeps MCP tools when --tools names no MCP tool` / `removes MCP tools
//! with --no-tools` / `does not declare unnamed MCP tools restored from the
//! transcript` / `lets tool_search declare unnamed MCP tools when --tools
//! names it` / `filters MCP tools by the mcp__ entries of --tools` /
//! `removes MCP tools matching --exclude-tools patterns`) and
//! `test/suite/regressions/5109-exclude-tools.test.ts` (`matches allowlist
//! and denylist patterns`) @ 4a080252f (`04b97ef00`).

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
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-tool-selection-test-{}-{id}",
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
    exposure: ext::ToolExposure,
) -> ext::ToolDefinition {
    ext::ToolDefinition {
        name: name.to_owned(),
        label: name.to_owned(),
        description: description.to_owned(),
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
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
        execute: Arc::new(|_request, _ctx| Box::pin(async move { Ok(text_result("ok")) })),
        render_call: None,
        render_result: None,
    }
}

/// Registers the MCP-shaped fixture tools: `dynamic_tool` (direct), a
/// deferred and a direct `mcp__docs__*` tool, and a direct `mcp__docs__shot`.
fn tools_extension(fail_exposure: ext::ToolExposure) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        let fail_exposure = fail_exposure;
        Box::pin(async move {
            api.register_tool(extension_tool(
                "dynamic_tool",
                "A dynamic tool",
                ext::ToolExposure::Direct,
            ))
            .map_err(|error| error.to_string())?;
            api.register_tool(extension_tool(
                "mcp__docs__search",
                "Search the docs",
                ext::ToolExposure::Deferred,
            ))
            .map_err(|error| error.to_string())?;
            api.register_tool(extension_tool(
                "mcp__docs__fail",
                "Fail a docs operation",
                fail_exposure,
            ))
            .map_err(|error| error.to_string())?;
            api.register_tool(extension_tool(
                "mcp__docs__shot",
                "Show a docs screenshot",
                ext::ToolExposure::Direct,
            ))
            .map_err(|error| error.to_string())?;
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

#[derive(Default)]
struct FixtureOptions {
    tools: Option<Vec<String>>,
    exclude_tools: Option<Vec<String>>,
    fail_exposure: Option<ext::ToolExposure>,
}

impl FixtureOptions {
    fn tools(entries: &[&str]) -> Self {
        Self {
            tools: Some(entries.iter().map(|entry| (*entry).to_owned()).collect()),
            ..Default::default()
        }
    }
}

async fn session_fixture(options: FixtureOptions) -> Fixture {
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
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            vec!["builtin:tool-search".to_owned()],
            vec![
                rpi::extensions::tool_search::inline_extension(),
                tools_extension(options.fail_exposure.unwrap_or(ext::ToolExposure::Deferred)),
            ],
            false,
            false,
        )
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
        tools: options.tools,
        exclude_tools: options.exclude_tools,
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

fn all_tool_names(session: &AgentSession) -> Vec<String> {
    session
        .get_all_tools()
        .into_iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

fn sorted_active(session: &AgentSession) -> Vec<String> {
    let mut names = session.get_active_tool_names();
    names.sort();
    names
}

/// `keeps MCP tools when --tools names no MCP tool` (agent-session-mcp.test.ts):
/// a non-empty allowlist without `mcp__` entries keeps MCP tools registered
/// for codemode and tool_search; a direct MCP tool is declared only when the
/// allowlist names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allowlist_without_mcp_entries_keeps_mcp_tools_registered() {
    let fixture = session_fixture(FixtureOptions {
        fail_exposure: Some(ext::ToolExposure::Direct),
        ..FixtureOptions::tools(&["read", "tool_search"])
    })
    .await;
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["read".to_owned(), "tool_search".to_owned()]
    );
    let registered = all_tool_names(&fixture.session);
    for name in ["mcp__docs__search", "mcp__docs__fail", "mcp__docs__shot"] {
        assert!(registered.contains(&name.to_owned()), "{registered:?}");
    }
    assert!(!registered.contains(&"bash".to_owned()), "{registered:?}");

    // A direct MCP tool stays registered but is never declared while the
    // allowlist does not name it.
    let fixture = session_fixture(FixtureOptions {
        fail_exposure: Some(ext::ToolExposure::Direct),
        ..FixtureOptions::tools(&["read"])
    })
    .await;
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["read".to_owned()]
    );
    assert!(
        all_tool_names(&fixture.session).contains(&"mcp__docs__fail".to_owned()),
        "direct MCP tool stays registered for tool_search"
    );
}

/// `removes MCP tools with --no-tools`: an empty allowlist removes every
/// tool, MCP included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_allowlist_removes_mcp_tools() {
    let fixture = session_fixture(FixtureOptions {
        tools: Some(Vec::new()),
        ..Default::default()
    })
    .await;
    assert!(
        all_tool_names(&fixture.session).is_empty(),
        "{:?}",
        all_tool_names(&fixture.session)
    );
    assert!(fixture.session.get_active_tool_names().is_empty());
}

/// `filters MCP tools by the mcp__ entries of --tools`: once an entry starts
/// with `mcp__`, MCP tools are matched by the allowlist too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_pattern_entries_filter_mcp_tools() {
    let fixture = session_fixture(FixtureOptions {
        fail_exposure: Some(ext::ToolExposure::Direct),
        ..FixtureOptions::tools(&["read", "mcp__docs__s*"])
    })
    .await;
    assert_eq!(
        sorted_active(&fixture.session),
        vec!["mcp__docs__shot".to_owned(), "read".to_owned()]
    );
    let registered = all_tool_names(&fixture.session);
    assert!(registered.contains(&"mcp__docs__search".to_owned()));
    assert!(registered.contains(&"mcp__docs__shot".to_owned()));
    assert!(!registered.contains(&"mcp__docs__fail".to_owned()));
    assert!(!registered.contains(&"list_mcp_resources".to_owned()));
}

/// `removes MCP tools matching --exclude-tools patterns`: the denylist
/// applies to MCP tools and takes patterns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_patterns_remove_mcp_tools() {
    let fixture = session_fixture(FixtureOptions {
        exclude_tools: Some(vec!["mcp__docs__f*".to_owned()]),
        ..FixtureOptions::tools(&["read"])
    })
    .await;
    let registered = all_tool_names(&fixture.session);
    assert!(registered.contains(&"mcp__docs__search".to_owned()));
    assert!(!registered.contains(&"mcp__docs__fail".to_owned()));
}

/// `does not declare unnamed MCP tools restored from the transcript`:
/// activating a registered direct MCP tool by name is filtered by
/// `_isActivatable` while the allowlist does not name it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unnamed_direct_mcp_tool_stays_inactive_when_activated() {
    let fixture = session_fixture(FixtureOptions {
        fail_exposure: Some(ext::ToolExposure::Direct),
        ..FixtureOptions::tools(&["read"])
    })
    .await;
    fixture
        .session
        .set_active_tools_by_name(vec!["read".to_owned(), "mcp__docs__fail".to_owned()]);
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["read".to_owned()],
        "a direct MCP tool the allowlist keeps but does not name is not declared"
    );
}

/// `lets tool_search declare unnamed MCP tools when --tools names it`: a
/// deferred MCP tool becomes active after `tool_search` loads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_search_declares_unnamed_mcp_tools() {
    let fixture = session_fixture(FixtureOptions::tools(&["tool_search"])).await;
    let arguments = json!({ "query": "docs search" })
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

    assert!(
        fixture
            .session
            .get_active_tool_names()
            .contains(&"mcp__docs__search".to_owned()),
        "{:?}",
        fixture.session.get_active_tool_names()
    );
}

/// `matches allowlist and denylist patterns` (#5109): patterns that match a
/// registered tool activate it like its name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patterns_activate_matching_tools() {
    let fixture = session_fixture(FixtureOptions {
        exclude_tools: Some(vec!["ask*".to_owned()]),
        ..FixtureOptions::tools(&["*_tool", "re*"])
    })
    .await;
    assert_eq!(
        sorted_active(&fixture.session),
        vec!["dynamic_tool".to_owned(), "read".to_owned()]
    );
}
