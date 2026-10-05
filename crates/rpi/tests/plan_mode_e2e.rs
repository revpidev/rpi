//! TE43 FR-G — `rpi-plan-mode` full-chain e2e: the real cdylib through the
//! real `NativeExtensionHost` bound to a real `AgentSession` (faux
//! provider), covering the TE43 task-file §4 integration table:
//!
//! 1. `plan_toggle_…` — `/plan` entry/exit: non-whitelist tools hidden,
//!    whitelist active, hint widget on/off, restoration to natural
//!    exposures + the entry snapshot.
//! 2. `write_plan_approve_…` — end-to-end plan file write + approval
//!    dialog: summary injected as a follow-up, mode back to Default.
//! 3. `write_plan_revision_…` — the revise branch routes feedback through
//!    the tool result and stays in Plan mode.
//! 4. `write_plan_rejects_path_…` — the schema forbids extra arguments
//!    (structural path-injection immunity).
//! 5. `write_plan_no_ui_…` — the non-interactive degradation (no dialog,
//!    text note, stays in Plan mode).
//! 6. `session_reset_…` — the host `plan → default` reset notification
//!    clears the boundary and the hint.
//! 7. `late_tool_surface_…` — a tool registered mid-plan (subagents/MCP
//!    shape) is re-tightened and returns to its natural exposure on exit.
//! 8. `config_hot_change_…` — a `config.toml` edit applies at the next
//!    trigger, in both the hide and release directions.
//!
//! Environment: every test holds the process-wide `ENV_LOCK` (sandboxed
//! HOME/XDG/locale vars — the plugin reads
//! `$XDG_CONFIG_HOME/rpi-plan-mode/config.toml`), and the cdylib must
//! exist (`cargo build -p rpi-ext-plan-mode`), otherwise the tests skip
//! with a message (l0_load / todo_e2e precedents).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rpi_ext_host::api::{
    ExtensionWidgetOptions, NotifyType, SetThemeResult, TerminalInputHandler, ThemeInfo, UiBridge,
    UiDialogOptions, Unsubscribe, WidgetContent, WorkingIndicatorOptions,
};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::types::ComponentTree;
use rpi_test_support::faux::{
    FauxAiProvider, FauxAssistantOptions, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    FauxResponseStep, faux_assistant_message, faux_tool_call,
};
use serde_json::{Value, json};
use tokio::time::sleep;

// ---------------------------------------------------------------------------
// Environment sandbox (process-wide env mutations → tests serialize)
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn acquire(home: &Path) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let mut previous = Vec::new();
        let mut set = |name: &'static str, value: Option<String>| {
            previous.push((name, std::env::var(name).ok()));
            match value {
                Some(value) => rpi_test_env::set_var(name, value),
                None => rpi_test_env::remove_var(name),
            }
        };
        let home = home.to_string_lossy().into_owned();
        set("HOME", Some(home.clone()));
        set("USERPROFILE", Some(home));
        set("XDG_CONFIG_HOME", None);
        set("RPI_OFFLINE", Some("1".to_owned()));
        set("RPI_SKIP_VERSION_CHECK", Some("1".to_owned()));
        set("LANG", None);
        set("LC_ALL", None);
        set("LC_MESSAGES", None);
        set("VISUAL", None);
        set("EDITOR", None);
        EnvGuard {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..) {
            match value {
                Some(value) => rpi_test_env::set_var(name, value),
                None => rpi_test_env::remove_var(name),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sandbox dirs + plugin package
// ---------------------------------------------------------------------------

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!(
            "rpi-plan-mode-e2e-{tag}-{}-{nanos}",
            std::process::id()
        ));
        for dir in ["cwd", "agent", "sessions", "home", "extensions"] {
            std::fs::create_dir_all(root.join(dir)).expect("sandbox dir");
        }
        Sandbox { root }
    }

    fn cwd(&self) -> PathBuf {
        self.root.join("cwd")
    }
    fn agent_dir(&self) -> PathBuf {
        self.root.join("agent")
    }
    fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Write the plugin config at the TE-D45 XDG path inside the sandbox.
    fn write_config(&self, toml: &str) {
        let dir = self.home().join(".config/rpi-plan-mode");
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(dir.join("config.toml"), toml).expect("config write");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn plugin_path() -> Option<PathBuf> {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    let name = if cfg!(target_os = "macos") {
        "librpi_ext_plan_mode.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_plan_mode.dll"
    } else {
        "librpi_ext_plan_mode.so"
    };
    let plugin = target.join("debug").join(name);
    if plugin.is_file() {
        Some(plugin)
    } else {
        eprintln!(
            "skipping: cdylib missing at {} — build with `cargo build -p rpi-ext-plan-mode`",
            plugin.display()
        );
        None
    }
}

/// Install the cdylib + the crate's real manifest under the production
/// discovery root (`<agent_dir>/extensions/<name>/`).
fn install_plugin_for_discovery(sandbox: &Sandbox, plugin: &Path) {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/rpi-ext-plan-mode/rpi-extension.json"),
        )
        .expect("crate manifest"),
    )
    .expect("manifest json");
    let dir = sandbox.agent_dir().join("extensions").join("rpi-plan-mode");
    std::fs::create_dir_all(&dir).expect("extensions dir");
    let native = manifest["native"].as_str().expect("native").to_owned();
    std::fs::copy(plugin, dir.join(native)).expect("copy cdylib");
    std::fs::write(
        dir.join("rpi-extension.json"),
        serde_json::to_string_pretty(&manifest).expect("manifest"),
    )
    .expect("write manifest");
}

// ---------------------------------------------------------------------------
// Scripted dialog + recording bridge
// ---------------------------------------------------------------------------

type WidgetRecord = (
    String,
    Option<WidgetContent>,
    Option<ExtensionWidgetOptions>,
);

#[derive(Default)]
struct ScriptedBridge {
    selects: Mutex<VecDeque<Value>>,
    inputs: Mutex<VecDeque<Value>>,
    select_titles: Mutex<Vec<String>>,
    widgets: Mutex<Vec<WidgetRecord>>,
    notifies: Mutex<Vec<(String, NotifyType)>>,
}

impl ScriptedBridge {
    fn queue_select(&self, answer: Value) {
        self.selects
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(answer);
    }

    fn queue_input(&self, answer: Value) {
        self.inputs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(answer);
    }

    fn select_titles(&self) -> Vec<String> {
        self.select_titles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn widget_present(&self, key_suffix: &str) -> bool {
        let widgets = self
            .widgets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        widgets
            .iter()
            .rev()
            .find(|(key, _, _)| key.ends_with(key_suffix))
            .is_some_and(|(_, content, _)| content.is_some())
    }

    fn notifications(&self) -> Vec<(String, NotifyType)> {
        self.notifies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl UiBridge for ScriptedBridge {
    async fn select(
        &self,
        title: &str,
        _options: &[String],
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        self.select_titles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(title.to_owned());
        self.selects
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
            .and_then(|answer| answer.as_str().map(str::to_owned))
    }
    async fn confirm(&self, _t: &str, _m: &str, _o: Option<UiDialogOptions>) -> bool {
        false
    }
    async fn input(
        &self,
        _title: &str,
        _placeholder: Option<&str>,
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        self.inputs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
            .and_then(|answer| answer.as_str().map(str::to_owned))
    }
    fn notify(&self, message: &str, kind: NotifyType) {
        self.notifies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((message.to_owned(), kind));
    }
    fn on_terminal_input(&self, _handler: TerminalInputHandler) -> Unsubscribe {
        Box::new(|| {})
    }
    fn set_status(&self, _k: &str, _t: Option<&str>) {}
    fn set_working_message(&self, _m: Option<&str>) {}
    fn set_working_visible(&self, _v: bool) {}
    fn set_working_indicator(&self, _o: Option<WorkingIndicatorOptions>) {}
    fn set_hidden_thinking_label(&self, _l: Option<&str>) {}
    fn set_widget(
        &self,
        key: &str,
        content: Option<WidgetContent>,
        options: Option<ExtensionWidgetOptions>,
    ) {
        self.widgets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((key.to_owned(), content, options));
    }
    fn set_footer(&self, _c: Option<ComponentTree>) {}
    fn set_header(&self, _c: Option<ComponentTree>) {}
    fn set_title(&self, _t: &str) {}
    async fn custom(&self, _c: ComponentTree, _o: Option<Value>) -> Option<Value> {
        None
    }
    fn paste_to_editor(&self, _t: &str) {}
    fn set_editor_text(&self, _t: &str) {}
    fn get_editor_text(&self) -> String {
        String::new()
    }
    async fn editor(&self, _t: &str, _p: Option<&str>) -> Option<String> {
        None
    }
    fn add_autocomplete_provider(&self, _p: Value) {}
    fn set_editor_component(&self, _c: Option<ComponentTree>) {}
    fn get_editor_component(&self) -> Option<ComponentTree> {
        None
    }
    fn theme(&self) -> Value {
        json!({})
    }
    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        Vec::new()
    }
    fn get_theme(&self, _n: &str) -> Option<Value> {
        None
    }
    fn set_theme(&self, _t: Value) -> SetThemeResult {
        SetThemeResult {
            success: false,
            error: None,
        }
    }
    fn get_tools_expanded(&self) -> bool {
        false
    }
    fn set_tools_expanded(&self, _expanded: bool) {}
}

// ---------------------------------------------------------------------------
// Session fixture (real host + real AgentSession + faux provider)
// ---------------------------------------------------------------------------

struct Fixture {
    /// Kept alive for the extension pipeline; the session holds its own
    /// handle (tests drive the session, not the host directly).
    _host: Arc<NativeExtensionHost>,
    session: rpi::core::agent_session::AgentSession,
    bridge: Arc<ScriptedBridge>,
    sandbox: Sandbox,
}

async fn build_model_runtime(
    agent_dir: &Path,
    steps: Vec<FauxResponseStep>,
) -> (
    Arc<rpi::core::model_runtime::ModelRuntime>,
    rpi_ai::types::Model,
) {
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
    provider.set_responses(steps);
    let model = provider.get_model(None).expect("faux model");
    let runtime = rpi::core::model_runtime::ModelRuntime::create(
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
    runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider)))
        .await
        .expect("register faux provider");
    (runtime, model)
}

/// Boot the host with the real plugin and a real file-backed session.
/// The caller owns the sandbox (and the matching [`EnvGuard`]) so the
/// plugin's XDG config path lands inside it.
async fn boot(
    sandbox: Sandbox,
    inline: Vec<rpi_ext_host::loader::InlineExtension>,
    steps: Vec<FauxResponseStep>,
    ui: bool,
) -> Fixture {
    let plugin = plugin_path().expect("the caller checked the cdylib");
    install_plugin_for_discovery(&sandbox, &plugin);
    let host = Arc::new(NativeExtensionHost::new(&sandbox.cwd().to_string_lossy()));
    let errors = host
        .load_startup_final(
            sandbox.agent_dir(),
            Vec::new(),
            Vec::new(),
            inline,
            true,
            false,
        )
        .await;
    assert!(errors.is_empty(), "plugin load errors: {errors:?}");

    let bridge = Arc::new(ScriptedBridge::default());
    host.set_ui(
        if ui {
            Some(bridge.clone() as Arc<dyn UiBridge>)
        } else {
            None
        },
        rpi_ext_host::types::ExtensionMode::Tui,
    );

    let (model_runtime, model) = build_model_runtime(&sandbox.agent_dir(), steps).await;
    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: sandbox.cwd(),
            agent_dir: Some(sandbox.agent_dir()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");
    let session_manager = rpi::core::session_manager::SessionManager::create(
        &sandbox.cwd(),
        Some(&sandbox.sessions()),
        rpi::core::session_manager::NewSessionOptions::default(),
    )
    .expect("file-backed session");
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(sandbox.cwd()),
        agent_dir: Some(sandbox.agent_dir()),
        model_runtime: None,
        model: Some(model),
        services: Some(services),
        session_manager: Some(Arc::new(Mutex::new(session_manager))),
        extension_host: Some(host.clone()),
        ..Default::default()
    })
    .await
    .expect("create session");
    let session = created.session;
    rpi::core::extension_actions::bind_session_actions(&host, &session).await;
    session
        .bind_extensions(rpi::core::agent_session::ExtensionBindings {
            mode: Some(rpi::core::extensions::ExtensionMode::Interactive),
            on_error: None,
            shutdown: None,
        })
        .await;

    Fixture {
        _host: host,
        session,
        bridge,
        sandbox,
    }
}

async fn await_idle(session: &rpi::core::agent_session::AgentSession, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if session.is_idle() {
            return;
        }
        sleep(Duration::from_millis(25)).await;
    }
    panic!("session never went idle");
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return;
        }
        assert!(Instant::now() < deadline, "condition never became true");
        sleep(Duration::from_millis(20)).await;
    }
}

async fn turn(session: &rpi::core::agent_session::AgentSession, text: &str) {
    session
        .prompt(text, rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    await_idle(session, Duration::from_secs(30)).await;
}

fn write_plan_step(content: &str) -> FauxResponseStep {
    let mut arguments = serde_json::Map::new();
    arguments.insert("content".to_owned(), json!(content));
    faux_assistant_message(
        faux_tool_call("write_plan", arguments, None),
        FauxAssistantOptions::default(),
    )
    .into()
}

fn write_plan_step_with_path(content: &str, path: &str) -> FauxResponseStep {
    let mut arguments = serde_json::Map::new();
    arguments.insert("content".to_owned(), json!(content));
    arguments.insert("path".to_owned(), json!(path));
    faux_assistant_message(
        faux_tool_call("write_plan", arguments, None),
        FauxAssistantOptions::default(),
    )
    .into()
}

fn text_step(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

/// Effective exposure of one tool from the live session registry.
fn exposure_of(session: &rpi::core::agent_session::AgentSession, name: &str) -> Option<String> {
    session
        .get_all_tools()
        .into_iter()
        .find(|tool| tool["name"] == name)
        .and_then(|tool| tool["exposure"].as_str().map(str::to_owned))
}

fn plan_tool_results(session: &rpi::core::agent_session::AgentSession) -> Vec<Value> {
    session
        .messages()
        .into_iter()
        .filter_map(|message| serde_json::to_value(message).ok())
        .filter(|value| value["role"] == "toolResult" && value["toolName"] == "write_plan")
        .collect()
}

async fn execute_plan_command(session: &rpi::core::agent_session::AgentSession, args: &str) {
    let executed = session
        .extension_runner()
        .execute_extension_command("plan", args)
        .await;
    assert!(executed, "/plan resolves to the plugin");
}

/// Enter Plan mode and wait for the boundary to land.
async fn enter_plan(fixture: &Fixture) {
    execute_plan_command(&fixture.session, "").await;
    wait_until(Duration::from_secs(5), || {
        exposure_of(&fixture.session, "edit").as_deref() == Some("hidden")
    })
    .await;
}

// ---------------------------------------------------------------------------
// FR-G 1 — enter/exit boundary
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_toggle_hides_and_restores_the_boundary() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("toggle");
    let _env = EnvGuard::acquire(&sandbox.home());
    let fixture = boot(sandbox, Vec::new(), Vec::new(), true).await;
    let session = &fixture.session;

    // Natural state before entering.
    assert_eq!(exposure_of(session, "edit").as_deref(), Some("direct"));
    assert_eq!(
        exposure_of(session, "write_plan").as_deref(),
        Some("direct")
    );
    assert!(
        !session
            .get_active_tool_names()
            .iter()
            .any(|name| name == "write_plan"),
        "write_plan defaults to inactive outside Plan mode"
    );
    assert!(!fixture.bridge.widget_present("plan-mode-hint"));

    enter_plan(&fixture).await;
    assert_eq!(
        session.permission_mode(),
        rpi::core::permission_mode::PermissionMode::Plan
    );

    // Non-whitelist tools are hidden; the whitelist keeps natural exposures.
    for name in ["bash", "edit", "write"] {
        assert_eq!(
            exposure_of(session, name).as_deref(),
            Some("hidden"),
            "{name} must be hidden in Plan mode"
        );
    }
    for name in ["read", "grep", "find", "ls", "write_plan"] {
        assert_eq!(
            exposure_of(session, name).as_deref(),
            Some("direct"),
            "{name} stays direct"
        );
    }

    // Active set = whitelist; hidden tools are not callable; the model
    // declaration no longer carries them.
    let active = session.get_active_tool_names();
    assert!(active.iter().any(|name| name == "read"));
    assert!(active.iter().any(|name| name == "write_plan"));
    assert!(!active.iter().any(|name| name == "edit"), "{active:?}");
    assert!(!active.iter().any(|name| name == "bash"), "{active:?}");
    let callable = session.get_callable_tool_names();
    assert!(!callable.iter().any(|name| name == "edit"), "{callable:?}");
    assert!(!callable.iter().any(|name| name == "write"), "{callable:?}");
    assert!(!callable.iter().any(|name| name == "bash"), "{callable:?}");
    let prompt = session.system_prompt();
    assert!(
        !prompt.contains("- edit:") && !prompt.contains("- bash:"),
        "the tools section drops hidden declarations"
    );

    // The editor hint widget is up.
    assert!(
        fixture.bridge.widget_present("plan-mode-hint"),
        "plan-mode hint registered"
    );

    // `/plan status` reports mode + plan file through ui.notify.
    execute_plan_command(session, "status").await;
    let last = fixture
        .bridge
        .notifications()
        .pop()
        .expect("status notification");
    assert!(last.0.contains("plan mode: plan"), "{}", last.0);
    assert!(last.0.contains(session.session_id().as_str()), "{}", last.0);

    // Exit: `/plan` again restores natural exposures + the entry snapshot.
    execute_plan_command(session, "").await;
    wait_until(Duration::from_secs(5), || {
        exposure_of(session, "edit").as_deref() == Some("direct")
    })
    .await;
    assert_eq!(
        session.permission_mode(),
        rpi::core::permission_mode::PermissionMode::Default
    );
    for name in ["bash", "edit", "write"] {
        assert_eq!(
            exposure_of(session, name).as_deref(),
            Some("direct"),
            "{name} returns to its natural exposure"
        );
    }
    let active = session.get_active_tool_names();
    assert!(active.iter().any(|name| name == "edit"), "{active:?}");
    assert!(active.iter().any(|name| name == "bash"), "{active:?}");
    assert!(
        !fixture.bridge.widget_present("plan-mode-hint"),
        "the hint widget is removed"
    );
}

// ---------------------------------------------------------------------------
// FR-G 2 — write_plan end-to-end + approval
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_plan_approve_writes_the_file_and_injects_the_summary() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("approve");
    let _env = EnvGuard::acquire(&sandbox.home());
    let fixture = boot(
        sandbox,
        Vec::new(),
        vec![
            write_plan_step("# Plan\n\n1. do the thing"),
            text_step("plan written"),
            text_step("executing the approved plan"),
        ],
        true,
    )
    .await;
    let session = &fixture.session;
    enter_plan(&fixture).await;

    fixture.bridge.queue_select(json!("Approve and execute"));

    turn(session, "plan the change").await;

    let titles = fixture.bridge.select_titles();
    assert_eq!(titles.len(), 1, "one review dialog");
    assert!(titles[0].contains("Review the plan"), "{}", titles[0]);

    let session_id = session.session_id();
    let plan_path = fixture
        .sandbox
        .cwd()
        .join(".rpi/plans")
        .join(format!("{session_id}-1.md"));
    assert_eq!(
        std::fs::read_to_string(&plan_path).expect("plan file written"),
        "# Plan\n\n1. do the thing"
    );

    let results = plan_tool_results(session);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["details"]["outcome"], "approved");
    assert!(!results[0]["isError"].as_bool().unwrap_or(false));

    // Approval left Plan mode, restored the tools, and injected the plan
    // summary as a follow-up user message.
    assert_eq!(
        session.permission_mode(),
        rpi::core::permission_mode::PermissionMode::Default
    );
    wait_until(Duration::from_secs(5), || {
        exposure_of(session, "edit").as_deref() == Some("direct")
    })
    .await;
    let injected = || {
        session.messages().into_iter().any(|message| {
            serde_json::to_value(&message).ok().is_some_and(|value| {
                if value["role"] != "user" {
                    return false;
                }
                let text: String = match &value["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(blocks) => blocks
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                text.contains("Start executing it now") && text.contains("do the thing")
            })
        })
    };
    wait_until(Duration::from_secs(10), injected).await;
}

// ---------------------------------------------------------------------------
// FR-G 3 — revision branch
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_plan_revision_routes_feedback_and_stays_in_plan_mode() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("revise");
    let _env = EnvGuard::acquire(&sandbox.home());
    let fixture = boot(
        sandbox,
        Vec::new(),
        vec![write_plan_step("v1 plan"), text_step("revised")],
        true,
    )
    .await;
    let session = &fixture.session;
    enter_plan(&fixture).await;

    fixture.bridge.queue_select(json!("Continue revising"));
    fixture.bridge.queue_input(json!("add rollback notes"));

    turn(session, "plan it").await;

    let results = plan_tool_results(session);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["details"]["outcome"], "revised");
    let text = results[0]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("add rollback notes"), "{text}");
    assert_eq!(
        session.permission_mode(),
        rpi::core::permission_mode::PermissionMode::Plan,
        "a revision stays in Plan mode"
    );
    assert_eq!(exposure_of(session, "edit").as_deref(), Some("hidden"));
}

// ---------------------------------------------------------------------------
// FR-G 3b — abandon/Esc branch
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_plan_abandon_leaves_plan_mode_without_injection() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("abandon");
    let _env = EnvGuard::acquire(&sandbox.home());
    let fixture = boot(
        sandbox,
        Vec::new(),
        vec![write_plan_step("abandoned plan"), text_step("dropped")],
        true,
    )
    .await;
    let session = &fixture.session;
    enter_plan(&fixture).await;

    // Esc is a `null` selection (the host dialog's cancel answer).
    fixture.bridge.queue_select(Value::Null);

    turn(session, "plan then cancel").await;

    let results = plan_tool_results(session);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["details"]["outcome"], "abandoned");
    assert_eq!(
        session.permission_mode(),
        rpi::core::permission_mode::PermissionMode::Default
    );
    wait_until(Duration::from_secs(5), || {
        exposure_of(session, "edit").as_deref() == Some("direct")
    })
    .await;
    let injected = session.messages().into_iter().any(|message| {
        serde_json::to_value(&message)
            .ok()
            .is_some_and(|value| value["role"] == "user")
    });
    // Only the original prompt user message exists (no summary injection).
    let user_messages = session
        .messages()
        .into_iter()
        .filter_map(|message| serde_json::to_value(&message).ok())
        .filter(|value| value["role"] == "user")
        .count();
    assert!(injected);
    assert_eq!(user_messages, 1, "abandon injects nothing");
}

// ---------------------------------------------------------------------------
// FR-G 4 — schema rejects a path argument
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_plan_rejects_extra_path_arguments() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("schema");
    let _env = EnvGuard::acquire(&sandbox.home());
    let fixture = boot(
        sandbox,
        Vec::new(),
        vec![
            write_plan_step_with_path("# Plan", "/tmp/rpi-plan-mode-evil.md"),
            text_step("rejected"),
        ],
        true,
    )
    .await;
    let session = &fixture.session;
    enter_plan(&fixture).await;

    turn(session, "try a path").await;

    let results = plan_tool_results(session);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(
        results[0]["isError"], true,
        "the extra path argument must fail schema validation: {results:?}"
    );
    assert!(
        !Path::new("/tmp/rpi-plan-mode-evil.md").exists(),
        "the injected path is never written"
    );
    assert!(
        fixture.bridge.select_titles().is_empty(),
        "a rejected call never reaches the review dialog"
    );
}

// ---------------------------------------------------------------------------
// FR-G 5 — non-interactive degradation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_plan_without_ui_degrades_to_a_text_note() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("noui");
    let _env = EnvGuard::acquire(&sandbox.home());
    let fixture = boot(
        sandbox,
        Vec::new(),
        vec![write_plan_step("deferred plan"), text_step("saved")],
        false,
    )
    .await;
    let session = &fixture.session;
    enter_plan(&fixture).await;

    turn(session, "plan without ui").await;

    let results = plan_tool_results(session);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["details"]["outcome"], "pending");
    let text = results[0]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("review"), "{text}");
    assert_eq!(
        session.permission_mode(),
        rpi::core::permission_mode::PermissionMode::Plan,
        "degradation keeps Plan mode"
    );
}

// ---------------------------------------------------------------------------
// FR-G 6 — session reset clears the boundary
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_reset_notification_clears_the_plan_boundary() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("reset");
    let _env = EnvGuard::acquire(&sandbox.home());
    let fixture = boot(sandbox, Vec::new(), Vec::new(), true).await;
    let session = &fixture.session;
    enter_plan(&fixture).await;

    // The interactive reset path stores Default and dispatches
    // `plan → default` (the `/new` session-rebind notification shape).
    session.set_permission_mode(rpi::core::permission_mode::PermissionMode::Default);

    wait_until(Duration::from_secs(5), || {
        exposure_of(session, "edit").as_deref() == Some("direct")
    })
    .await;
    assert_eq!(exposure_of(session, "bash").as_deref(), Some("direct"));
    assert!(
        !fixture.bridge.widget_present("plan-mode-hint"),
        "the reset removes the hint"
    );
    let active = session.get_active_tool_names();
    assert!(active.iter().any(|name| name == "edit"), "{active:?}");
}

// ---------------------------------------------------------------------------
// FR-G 7 — late tool surface (subagents/MCP shape)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_registered_tools_are_re_tightened_and_restored() {
    use rpi_ext_host::api::ExtensionApi;
    use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};

    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("late");
    let _env = EnvGuard::acquire(&sandbox.home());

    // An inline extension loaded at boot keeps an `ExtensionApi` handle
    // the test uses to register a mutating tool mid-plan (the
    // subagents/MCP shape; the same surface V16-14's integration tests
    // drive).
    let api_slot: Arc<Mutex<Option<ExtensionApi>>> = Arc::new(Mutex::new(None));
    let slot_for_factory = api_slot.clone();
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        *slot_for_factory
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(api);
        Box::pin(async { Ok(()) })
    });
    let fixture = boot(
        sandbox,
        vec![InlineExtension::Anonymous(factory)],
        Vec::new(),
        true,
    )
    .await;
    let session = &fixture.session;
    enter_plan(&fixture).await;

    let api = api_slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .expect("inline factory ran");
    api.register_tool(rpi_ext_host::types::ToolDefinition {
        name: "late_mutator".to_owned(),
        label: "late_mutator".to_owned(),
        description: "late mutating tool".to_owned(),
        prompt_snippet: Some("late_mutator: mutates state".to_owned()),
        prompt_guidelines: None,
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
        output_schema: None,
        exposure: rpi_ext_host::types::ToolExposure::Direct,
        namespace: None,
        annotations: None,
        default_active: Some(true),
        prepare_loadout: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_request, _ctx| {
            Box::pin(async { Ok(rpi_agent::types::AgentToolResult::default()) })
        }),
        render_call: None,
        render_result: None,
    })
    .expect("register late tool");

    // The dynamic-surface trigger (mcp_servers_change) re-tightens.
    session
        .extension_runner()
        .emit_event("mcp_servers_change", json!({}))
        .await;
    wait_until(Duration::from_secs(5), || {
        exposure_of(session, "late_mutator").as_deref() == Some("hidden")
    })
    .await;
    assert!(
        !session
            .get_active_tool_names()
            .iter()
            .any(|name| name == "late_mutator"),
        "the late tool leaves the active set"
    );
    assert!(
        !session
            .get_callable_tool_names()
            .iter()
            .any(|name| name == "late_mutator"),
        "hidden tools are not callable"
    );
    assert!(
        !session
            .system_prompt()
            .contains("late_mutator: mutates state"),
        "the declaration drops the late tool"
    );

    // Exit restores it to its natural exposure.
    execute_plan_command(session, "").await;
    wait_until(Duration::from_secs(5), || {
        exposure_of(session, "late_mutator").as_deref() == Some("direct")
    })
    .await;
}

// ---------------------------------------------------------------------------
// FR-G 8 — config hot change
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_hot_change_applies_at_the_next_trigger() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("config");
    let _env = EnvGuard::acquire(&sandbox.home());
    let fixture = boot(
        sandbox,
        Vec::new(),
        vec![text_step("noted"), text_step("noted again")],
        true,
    )
    .await;
    let session = &fixture.session;
    fixture
        .sandbox
        .write_config("allowTools = [\"read\", \"bash\"]");
    enter_plan(&fixture).await;

    // bash is allowed by the config: stays direct. edit is not: hidden.
    assert_eq!(exposure_of(session, "bash").as_deref(), Some("direct"));
    assert_eq!(exposure_of(session, "edit").as_deref(), Some("hidden"));
    assert!(
        session
            .get_active_tool_names()
            .iter()
            .any(|name| name == "bash")
    );

    // Hot edit: bash leaves the allow list, edit joins it. The next
    // before_agent_start trigger (a prompt turn) applies it.
    fixture
        .sandbox
        .write_config("allowTools = [\"read\", \"edit\"]");
    turn(session, "recheck the boundary").await;
    assert_eq!(
        exposure_of(session, "bash").as_deref(),
        Some("hidden"),
        "removed names are hidden at the next trigger"
    );
    assert_eq!(
        exposure_of(session, "edit").as_deref(),
        Some("direct"),
        "re-allowed names are released at the next trigger"
    );
    let active = session.get_active_tool_names();
    assert!(active.iter().any(|name| name == "edit"), "{active:?}");
    assert!(!active.iter().any(|name| name == "bash"), "{active:?}");
}
