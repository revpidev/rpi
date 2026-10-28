//! Process-wide codemode worker admission (V18-03 FR-B; upstream
//! `packages/coding-agent/test/codemode-worker-admission.test.ts`
//! @ 4a080252f). This is its own test binary so the process-global pool and
//! `RPI_CODEMODE_MAX_WORKERS` changes cannot race other suites.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi::core::agent_session::AgentSession;
use rpi::core::extension_actions::bind_session_actions;
use rpi::extensions::codemode::{CodemodeMode, CodemodeSettings, workers};
use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_ext_host::types as ext;
use rpi_test_support::faux::{
    FauxAiProvider, FauxAssistantOptions, FauxContent, FauxModelDefinition, FauxProvider,
    FauxProviderOptions, FauxResponseStep, faux_assistant_message, faux_text, faux_tool_call,
};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rpi-codemode-workers-{}-{id}", std::process::id()));
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

/// The blocking tool every worker in this test calls: it reports that it
/// started, then holds its worker until the test releases one permit.
struct Gate {
    started: AtomicUsize,
    started_notify: Notify,
    permits: Semaphore,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            started: AtomicUsize::new(0),
            started_notify: Notify::new(),
            permits: Semaphore::new(0),
        }
    }
}

impl Gate {
    async fn hold(&self) {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.started_notify.notify_waiters();
        let permit = self.permits.acquire().await.expect("gate open");
        permit.forget();
    }

    fn release(&self) {
        self.permits.add_permits(1);
    }

    async fn wait_until_started(&self, count: usize) {
        for _ in 0..1000 {
            if self.started.load(Ordering::SeqCst) >= count {
                return;
            }
            let notified = self.started_notify.notified();
            if self.started.load(Ordering::SeqCst) >= count {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), notified).await;
        }
        panic!(
            "only {} of {count} gated workers started",
            self.started.load(Ordering::SeqCst)
        );
    }
}

fn gate_extension(gate: Arc<Gate>) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        let gate = gate.clone();
        Box::pin(async move {
            api.register_tool(ext::ToolDefinition {
                name: "gate".to_owned(),
                label: "gate".to_owned(),
                description: "Hold the worker until the test releases it.".to_owned(),
                prompt_snippet: None,
                prompt_guidelines: None,
                parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
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
                execute: Arc::new(move |_request, _ctx| {
                    let gate = gate.clone();
                    Box::pin(async move {
                        gate.hold().await;
                        Ok(text_result("gate"))
                    })
                }),
                render_call: None,
                render_result: None,
                reaches_tools: None,
            })
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

async fn fixture(gate: Arc<Gate>) -> Fixture {
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
        mode: CodemodeMode::On,
        inline_budget: None,
    }));
    let settings_fn = {
        let settings = settings.clone();
        Arc::new(move || settings.lock().unwrap().clone())
    };
    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let errors = host
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            vec!["builtin:codemode".to_owned()],
            vec![
                rpi::extensions::codemode::inline_extension(settings_fn, model_runtime.clone()),
                gate_extension(gate),
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
    created
        .session
        .set_active_tools_by_name(vec!["codemode".to_owned(), "gate".to_owned()]);
    Fixture {
        session: created.session,
        provider,
        _tmp: tmp,
    }
}

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
    fixture
        .session
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

/// The upstream admission fixture sets `PI_CODEMODE_MAX_WORKERS`; the rpi
/// harness maps the key at this boundary (ADR-0001) and the runtime reads
/// only the RPI spelling.
fn rpi_env_key(upstream_key: &str) -> String {
    upstream_key.replacen("PI_", "RPI_", 1)
}

fn set_workers(upstream_value: &str) {
    rpi_test_env::set_var(rpi_env_key("PI_CODEMODE_MAX_WORKERS"), upstream_value);
}

async fn run_gated(gate: Arc<Gate>) -> rpi_agent::types::AgentToolResult {
    let fixture = fixture(gate).await;
    run(&fixture, "return await tools.gate({});").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queues_scripts_at_the_process_wide_worker_limit() {
    set_workers("2");
    assert_eq!(workers::max_workers(), 2);

    // The second slot is filled by another fixture (a different "session",
    // like an in-process subagent) and both count.
    let gate = Arc::new(Gate::default());
    let held_one = tokio::spawn(run_gated(gate.clone()));
    let held_two = tokio::spawn(run_gated(gate.clone()));
    gate.wait_until_started(2).await;

    // A script whose own deadline is shorter than the 60 s wait reports that
    // timeout while it waits for a free worker.
    let timed_out = {
        let gate = gate.clone();
        tokio::spawn(async move {
            let fixture = fixture(gate).await;
            run(&fixture, "// @options: {\"timeout_ms\": 150}\nreturn 1").await
        })
    };
    let result = tokio::time::timeout(Duration::from_secs(10), timed_out)
        .await
        .expect("waiting script settles")
        .expect("task");
    let text = result_text(&result);
    assert!(result.is_error == Some(true), "{text}");
    assert!(
        text.contains("Execution timed out after 150 ms while waiting for a free codemode worker"),
        "{text}"
    );
    assert!(text.contains("The script did not start."), "{text}");

    // A plain script queues until an actual worker exits; its result must not
    // arrive before the release.
    let waiting = tokio::spawn(run_gated_plain(gate.clone()));
    // Wait until the third script is really queued before raising the limit.
    for _ in 0..200 {
        if workers::pool().waiting() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!waiting.is_finished(), "the third script must wait");
    assert_eq!(
        workers::pool().waiting(),
        1,
        "the third script must be queued"
    );

    // Raise the limit at runtime: the *queued* script is admitted first (FIFO,
    // upstream admitCodemodeWorkers), and a newcomer behind it still waits
    // because both original workers remain held.
    set_workers("3");
    assert_eq!(workers::max_workers(), 3);
    let extra = tokio::spawn(run_gated_plain(gate.clone()));
    let waiting_result = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("raised limit admits the queued script")
        .expect("task");
    assert!(
        waiting_result.is_error != Some(true),
        "{}",
        result_text(&waiting_result)
    );
    let extra_result = tokio::time::timeout(Duration::from_secs(10), extra)
        .await
        .expect("the newcomer settles after the admitted worker exits")
        .expect("task");
    assert!(
        extra_result.is_error != Some(true),
        "{}",
        result_text(&extra_result)
    );

    // Two releases free both held workers.
    gate.release();
    gate.release();
    let one = held_one.await.expect("held one");
    let two = held_two.await.expect("held two");
    assert!(one.is_error != Some(true), "{}", result_text(&one));
    assert!(two.is_error != Some(true), "{}", result_text(&two));

    // Invalid values fall back to two and are not an error at runtime.
    for invalid in ["0", "-1", "2.5", "NaN", "Infinity", "abc"] {
        set_workers(invalid);
        assert_eq!(
            workers::max_workers(),
            2,
            "RPI_CODEMODE_MAX_WORKERS={invalid:?}"
        );
    }
    // Raising the limit above the default is honored.
    set_workers("4");
    assert_eq!(workers::max_workers(), 4);
    // The upstream PI_ spelling is never read by the runtime.
    rpi_test_env::remove_var("RPI_CODEMODE_MAX_WORKERS");
    rpi_test_env::set_var("PI_CODEMODE_MAX_WORKERS", "9");
    assert_eq!(workers::max_workers(), 2);
    rpi_test_env::remove_var("PI_CODEMODE_MAX_WORKERS");
}

async fn run_gated_plain(gate: Arc<Gate>) -> rpi_agent::types::AgentToolResult {
    let fixture = fixture(gate).await;
    run(&fixture, "return 1").await
}
