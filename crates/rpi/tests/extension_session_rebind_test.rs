//! Session-replacement extension invariants (the `/new`-path shape from
//! interactive mode): every replaced session owns a fresh host whose tools
//! and `session_start` delivery survive, and the runner-downcast path the
//! UI rebind uses (`rebind_session_ui` -> `ExtensionHostAdapter::host()`)
//! resolves on the NEW session's runner.
//!
//! Anchors: agent-session-runtime.ts:133-215 (replacement ordering),
//! interactive-mode.ts:1817-1892 (`bindCurrentSessionExtensions` — the UI
//! context is re-attached on every switch; rpi attaches it on the host via
//! `set_ui`, commands_selectors.rs `rebind_session_ui`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rpi::core::agent_session::ExtensionBindings;
use rpi::core::extension_actions::bind_session_actions;
use rpi::core::extension_context::RuntimeCommandActions;
use rpi_ext_host::api::{CommandContextActions, ExtensionApi};
use rpi_ext_host::bridges::NullUiBridge;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::InlineExtension;
use rpi_ext_host::types as ext;
use rpi_test_support::faux::{
    faux_assistant_message, FauxAiProvider, FauxAssistantOptions, FauxModelDefinition,
    FauxProvider, FauxProviderOptions,
};
use serde_json::{json, Value};

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rpi-ext-rebind-{id}-{}", std::process::id()));
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
    runtime: rpi::core::agent_session_runtime::AgentSessionRuntime,
    hosts: Arc<Mutex<Vec<Arc<NativeExtensionHost>>>>,
    events: Arc<Mutex<Vec<String>>>,
    _tmp: TempDir,
}

/// Mirrors the app.rs `create_runtime` wiring: a FRESH host per creation,
/// extension tool registered at activate (the mcp-adapter shape), actions
/// bound right after session construction.
async fn fixture() -> Fixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let hosts: Arc<Mutex<Vec<Arc<NativeExtensionHost>>>> = Arc::new(Mutex::new(Vec::new()));
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let factory = {
        let hosts = hosts.clone();
        let events = events.clone();
        let agent_dir = agent_dir.clone();
        move |options: rpi::core::agent_session_runtime::CreateRuntimeOptions| {
            let hosts = hosts.clone();
            let events = events.clone();
            let agent_dir = agent_dir.clone();
            Box::pin(async move {
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
                provider.set_responses(vec![faux_assistant_message(
                    "ok",
                    FauxAssistantOptions::default(),
                )
                .into()]);
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
                    .expect("register faux");
                let services = rpi::core::agent_session_services::create_agent_session_services(
                    rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
                        cwd: options.cwd.clone(),
                        agent_dir: Some(agent_dir.clone()),
                        settings_manager: None,
                        model_runtime: Some(model_runtime.clone()),
                        extension_flag_values: Vec::new(),
                        resource_loader_options: None,
                    },
                )
                .await
                .expect("services");

                let events = events.clone();
                let inline = vec![InlineExtension::Anonymous(Arc::new(
                    move |api: ExtensionApi| {
                        api.register_tool(ext::ToolDefinition {
                            name: "probe_tool".to_owned(),
                            label: "probe_tool".to_owned(),
                            description: "probe".to_owned(),
                            prompt_snippet: None,
                            prompt_guidelines: None,
                            parameters: json!({"type": "object"}),
                            constrained_sampling: None,
                            render_shell: None,
                            prepare_arguments: None,
                            execution_mode: None,
                            execute: Arc::new(|_req, _ctx| {
                                Box::pin(async { Ok(rpi_agent::types::AgentToolResult::default()) })
                            }),
                            render_call: None,
                            render_result: None,
                        })
                        .expect("register probe_tool");
                        let events = events.clone();
                        api.on(
                            ext::EVENT_SESSION_START,
                            Arc::new(move |payload, _ctx| {
                                let reason = payload["reason"].as_str().unwrap_or("?").to_owned();
                                events
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .push(format!("start:{reason}"));
                                Box::pin(async { Ok(Value::Null) })
                            }),
                        )
                        .expect("on session_start");
                        Box::pin(async { Ok(()) })
                    },
                ))];
                let host = NativeExtensionHost::new(&options.cwd.to_string_lossy());
                let errors = host.load_inline(&inline).await;
                assert!(errors.is_empty(), "{errors:?}");
                let host = Arc::new(host);
                hosts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(host.clone());

                let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
                    cwd: Some(options.cwd.clone()),
                    agent_dir: Some(agent_dir.clone()),
                    model_runtime: Some(model_runtime),
                    model: Some(model),
                    services: Some(services.clone()),
                    session_manager: Some(options.session_manager),
                    session_start_event: options.session_start_event,
                    extension_host: Some(host.clone()),
                    ..Default::default()
                })
                .await
                .expect("create session");
                bind_session_actions(&host, &created.session).await;

                Ok(
                    rpi::core::agent_session_runtime::CreateAgentSessionRuntimeResult {
                        session: created.session,
                        services,
                        diagnostics: Vec::new(),
                        model_fallback_message: None,
                    },
                )
            })
                as futures::future::BoxFuture<
                    'static,
                    Result<
                        rpi::core::agent_session_runtime::CreateAgentSessionRuntimeResult,
                        rpi::error::RpiError,
                    >,
                >
        }
    };

    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let runtime = rpi::core::agent_session_runtime::create_agent_session_runtime(
        Arc::new(factory),
        rpi::core::agent_session_runtime::CreateRuntimeOptions {
            cwd: cwd.clone(),
            agent_dir,
            session_manager,
            session_start_event: None,
            project_trust_context: None,
        },
    )
    .await
    .expect("runtime");

    Fixture {
        runtime,
        hosts,
        events,
        _tmp: tmp,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_session_preserves_extension_tools_and_rebinds_ui_bridge() {
    let mut f = fixture().await;

    // Interactive `init()` (one-shot): bind_extensions + set_ui on the
    // initial host via the runner downcast.
    let session0 = f.runtime.session().clone();
    session0
        .bind_extensions(ExtensionBindings {
            mode: None,
            on_error: None,
            shutdown: None,
        })
        .await;
    let runner0 = session0.extension_runner();
    let host0 = runner0
        .as_any()
        .and_then(|any| {
            any.downcast_ref::<rpi::core::extension_host_adapter::ExtensionHostAdapter>()
        })
        .map(|adapter| adapter.host().clone())
        .expect("downcast to host adapter on the initial runner");
    host0.set_ui(Some(NullUiBridge::shared()), ext::ExtensionMode::Tui);
    assert!(host0.runtime().ui_bridge().is_some());
    assert!(session0
        .get_active_tool_names()
        .contains(&"probe_tool".to_owned()));

    // The production rebind (`rebind_session_ui`): bind_extensions, then
    // re-attach the UI bridge onto the NEW session's host through the same
    // downcast path.
    f.runtime.set_rebind_session(Some(Box::new(|session| {
        Box::pin(async move {
            session
                .bind_extensions(ExtensionBindings {
                    mode: None,
                    on_error: None,
                    shutdown: None,
                })
                .await;
            let runner = session.extension_runner();
            if let Some(host) = runner
                .as_any()
                .and_then(|any| {
                    any.downcast_ref::<rpi::core::extension_host_adapter::ExtensionHostAdapter>()
                })
                .map(|adapter| adapter.host().clone())
            {
                host.set_ui(Some(NullUiBridge::shared()), ext::ExtensionMode::Tui);
            }
        })
    })));

    let runtime = Arc::new(tokio::sync::Mutex::new(f.runtime));
    let actions = RuntimeCommandActions::new(&runtime);
    let cancelled = actions.new_session(None, None).await;
    assert!(!cancelled);
    let session1 = runtime.lock().await.session().clone();
    drop(actions);

    // Fresh host per replacement, and it is the one the new session's
    // runner points at.
    let hosts = f.hosts.lock().unwrap().clone();
    assert_eq!(hosts.len(), 2, "a fresh host per replaced session");
    let host1 = hosts.last().unwrap().clone();
    assert!(
        host1.runtime().ui_bridge().is_some(),
        "rebind_session_ui must re-attach the UI bridge to the new host"
    );

    // The extension pipeline is live on the new session: the tool survives
    // and session_start was delivered.
    let tools = session1.get_active_tool_names();
    assert!(
        tools.contains(&"probe_tool".to_owned()),
        "extension tools survive session replacement: {tools:?}"
    );
    let events = f.events.lock().unwrap().clone();
    assert!(events.contains(&"start:startup".to_owned()), "{events:?}");
    assert!(events.contains(&"start:new".to_owned()), "{events:?}");
}
