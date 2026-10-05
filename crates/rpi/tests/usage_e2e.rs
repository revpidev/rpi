//! TE44 FR-F — `rpi-usage` full-chain e2e: the real cdylib through the real
//! `NativeExtensionHost` bound to a real `AgentSession` (faux provider).
//!
//! The built-in Python scripts are shadowed by explicit `usage.providers`
//! settings pointing at shell stubs, so the whole chain runs offline while
//! still exercising: plugin install + provider registration, `/usage`
//! dispatch, the host framework fetch pipeline, envelope formatting, the
//! footer status write, and the failure-retention contract.
//!
//! Environment: every test holds the process-wide `ENV_LOCK` (sandboxed
//! HOME / agent dir), and the cdylib must exist (`cargo build -p
//! rpi-ext-usage`), otherwise the tests skip with a message (l0_load /
//! plan-mode e2e precedents). The file is unix-gated: the stubs are `sh`
//! scripts written with unix permissions (the host usage-framework tests
//! carry the same gate).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rpi_ext_host::api::{
    ExtensionWidgetOptions, NotifyType, SetThemeResult, TerminalInputHandler, ThemeInfo, UiBridge,
    UiDialogOptions, Unsubscribe, WidgetContent, WorkingIndicatorOptions,
};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::types::ComponentTree;
use rpi_test_support::faux::{
    FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Environment sandbox (process-wide env mutations → tests serialize)
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn acquire(home: &Path, agent_dir: &Path) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let mut previous = Vec::new();
        let mut set = |name: &'static str, value: Option<String>| {
            previous.push((name, std::env::var(name).ok()));
            match value {
                Some(value) => rpi_test_env::set_var(name, value),
                None => rpi_test_env::remove_var(name),
            }
        };
        set("HOME", Some(home.to_string_lossy().into_owned()));
        set("USERPROFILE", Some(home.to_string_lossy().into_owned()));
        set(
            "RPI_CODING_AGENT_DIR",
            Some(agent_dir.to_string_lossy().into_owned()),
        );
        set("RPI_OFFLINE", Some("1".to_owned()));
        set("RPI_SKIP_VERSION_CHECK", Some("1".to_owned()));
        set("XDG_CONFIG_HOME", None);
        set("LANG", None);
        set("LC_ALL", None);
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
            "rpi-usage-e2e-{tag}-{}-{nanos}",
            std::process::id()
        ));
        for dir in ["cwd", "agent", "sessions", "home", "stubs"] {
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

    /// Write a provider stub script (0755) and answer its path string.
    fn write_stub(&self, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = self.root.join("stubs").join(format!("{name}.sh"));
        std::fs::write(&path, format!("#!/bin/sh\ncat > /dev/null\n{body}\n")).expect("stub write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("stub chmod");
        path.to_string_lossy().into_owned()
    }

    /// Write the explicit `usage.providers` settings the host framework reads.
    fn write_settings(&self, provers: &[(&str, String)]) {
        let mut providers = serde_json::Map::new();
        for (provider, script) in provers {
            providers.insert((*provider).to_owned(), json!({ "script": script }));
        }
        let settings = json!({ "usage": { "providers": providers } });
        std::fs::write(
            self.agent_dir().join("settings.json"),
            serde_json::to_string_pretty(&settings).expect("settings json"),
        )
        .expect("settings write");
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
        "librpi_ext_usage.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_usage.dll"
    } else {
        "librpi_ext_usage.so"
    };
    let plugin = target.join("debug").join(name);
    if plugin.is_file() {
        Some(plugin)
    } else {
        eprintln!(
            "skipping: cdylib missing at {} — build with `cargo build -p rpi-ext-usage`",
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
                .join("../../crates/rpi-ext-usage/rpi-extension.json"),
        )
        .expect("crate manifest"),
    )
    .expect("manifest json");
    let dir = sandbox.agent_dir().join("extensions").join("rpi-usage");
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
// Recording UI bridge
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingBridge {
    notifications: Mutex<Vec<String>>,
    statuses: Mutex<Vec<(String, Option<String>)>>,
}

impl RecordingBridge {
    fn notifications(&self) -> Vec<String> {
        self.notifications
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn statuses(&self) -> Vec<(String, Option<String>)> {
        self.statuses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// The last write for one status key.
    fn last_status(&self, key: &str) -> Option<Option<String>> {
        self.statuses()
            .into_iter()
            .filter(|(entry, _)| entry == key)
            .map(|(_, text)| text)
            .next_back()
    }

    fn status_writes(&self, key: &str) -> Vec<Option<String>> {
        self.statuses()
            .into_iter()
            .filter(|(entry, _)| entry == key)
            .map(|(_, text)| text)
            .collect()
    }
}

#[async_trait]
impl UiBridge for RecordingBridge {
    async fn select(
        &self,
        _t: &str,
        _o: &[String],
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        None
    }
    async fn confirm(&self, _t: &str, _m: &str, _o: Option<UiDialogOptions>) -> bool {
        false
    }
    async fn input(
        &self,
        _t: &str,
        _p: Option<&str>,
        _o: Option<UiDialogOptions>,
    ) -> Option<String> {
        None
    }
    fn notify(&self, message: &str, _kind: NotifyType) {
        self.notifications
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(message.to_owned());
    }
    fn on_terminal_input(&self, _handler: TerminalInputHandler) -> Unsubscribe {
        Box::new(|| {})
    }
    fn set_status(&self, key: &str, text: Option<&str>) {
        self.statuses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((key.to_owned(), text.map(str::to_owned)));
    }
    fn set_working_message(&self, _m: Option<&str>) {}
    fn set_working_visible(&self, _v: bool) {}
    fn set_working_indicator(&self, _o: Option<WorkingIndicatorOptions>) {}
    fn set_hidden_thinking_label(&self, _l: Option<&str>) {}
    fn set_widget(&self, _k: &str, _c: Option<WidgetContent>, _o: Option<ExtensionWidgetOptions>) {}
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
    _host: Arc<NativeExtensionHost>,
    session: rpi::core::agent_session::AgentSession,
    bridge: Arc<RecordingBridge>,
    _sandbox: Sandbox,
}

const DEEPSEEK_ENVELOPE: &str = r#"printf '%s' '{"schemaVersion":1,"provider":"deepseek","displayText":"deepseek: CNY 9.99","balance":[{"currency":"CNY","total":9.99}]}'"#;
const GLM_ENVELOPE: &str = r#"printf '%s' '{"schemaVersion":1,"provider":"glm-coding-plan","plan":"pro","displayText":"glm-coding-plan: 5h 22% used","quota":{"used":22,"total":100,"unit":"%"},"resetAt":"2026-10-08T00:00:00Z"}'"#;

async fn build_model_runtime(
    agent_dir: &Path,
    provider_id: &str,
) -> (
    Arc<rpi::core::model_runtime::ModelRuntime>,
    rpi_ai::types::Model,
) {
    let provider = FauxProvider::new(FauxProviderOptions {
        provider: Some(provider_id.to_owned()),
        models: Some(vec![FauxModelDefinition {
            id: format!("{provider_id}-chat"),
            name: Some(format!("{provider_id} chat")),
            reasoning: Some(false),
            input: Some(vec![rpi_ai::types::InputModality::Text]),
            input_limits: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
    provider.set_responses(Vec::<FauxResponseStep>::new());
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
async fn boot(sandbox: Sandbox, inline: Vec<rpi_ext_host::loader::InlineExtension>) -> Fixture {
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

    let bridge = Arc::new(RecordingBridge::default());
    host.set_ui(
        Some(bridge.clone() as Arc<dyn UiBridge>),
        rpi_ext_host::types::ExtensionMode::Tui,
    );

    let (model_runtime, model) = build_model_runtime(&sandbox.agent_dir(), "deepseek").await;
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
    rpi::core::extension_actions::bind_session_actions(&host, &created.session).await;
    // The interactive host binds its extension mode at session start; the
    // bind emits `session_start`, which drives the plugin's initial footer
    // refresh and the coexistence inline handler.
    created
        .session
        .bind_extensions(rpi::core::agent_session::ExtensionBindings {
            mode: Some(rpi::core::extensions::ExtensionMode::Interactive),
            on_error: None,
            shutdown: None,
        })
        .await;

    Fixture {
        _host: host,
        session: created.session,
        bridge,
        _sandbox: sandbox,
    }
}

async fn wait_until<F: Fn() -> bool>(timeout: Duration, condition: F) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn run_usage(fixture: &Fixture, args: &str) {
    let executed = fixture
        .session
        .extension_runner()
        .execute_extension_command("usage", args)
        .await;
    assert!(executed, "/usage resolves to the plugin");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_command_reports_the_current_provider_and_updates_the_footer() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("cmd");
    let _env = EnvGuard::acquire(&sandbox.home(), &sandbox.agent_dir());
    let deepseek = sandbox.write_stub("deepseek", DEEPSEEK_ENVELOPE);
    sandbox.write_settings(&[("deepseek", deepseek)]);
    let fixture = boot(sandbox, Vec::new()).await;

    run_usage(&fixture, "").await;
    assert!(
        wait_until(Duration::from_secs(5), || {
            fixture
                .bridge
                .notifications()
                .iter()
                .any(|message| message.contains("deepseek: CNY 9.99"))
        })
        .await,
        "command report: {:?}",
        fixture.bridge.notifications()
    );
    let report = fixture
        .bridge
        .notifications()
        .into_iter()
        .find(|message| message.contains("deepseek: CNY 9.99"))
        .expect("report");
    assert!(report.contains("balance: CNY 9.99"), "{report}");
    assert!(
        wait_until(Duration::from_secs(5), || {
            fixture.bridge.last_status("rpi-usage") == Some(Some("deepseek: CNY 9.99".to_owned()))
        })
        .await,
        "footer status: {:?}",
        fixture.bridge.statuses()
    );
    // The plugin only ever writes its own status key.
    assert!(
        fixture
            .bridge
            .statuses()
            .iter()
            .all(|(key, _)| key == "rpi-usage"),
        "{:?}",
        fixture.bridge.statuses()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_all_isolates_a_failing_provider() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("all");
    let _env = EnvGuard::acquire(&sandbox.home(), &sandbox.agent_dir());
    let deepseek = sandbox.write_stub("deepseek", DEEPSEEK_ENVELOPE);
    let glm = sandbox.write_stub("glm", GLM_ENVELOPE);
    let kimi = sandbox.write_stub("kimi", "exit 1");
    sandbox.write_settings(&[
        ("deepseek", deepseek),
        ("glm-coding-plan", glm),
        ("kimi-code", kimi),
    ]);
    let fixture = boot(sandbox, Vec::new()).await;

    run_usage(&fixture, "all").await;
    assert!(
        wait_until(Duration::from_secs(5), || {
            fixture
                .bridge
                .notifications()
                .iter()
                .any(|message| message.contains("no data: kimi-code"))
        })
        .await,
        "report: {:?}",
        fixture.bridge.notifications()
    );
    let report = fixture
        .bridge
        .notifications()
        .into_iter()
        .find(|message| message.contains("no data: kimi-code"))
        .expect("report");
    assert!(report.contains("deepseek: CNY 9.99"), "{report}");
    assert!(report.contains("glm-coding-plan: 5h 22% used"), "{report}");
    assert!(report.contains("plan: pro"), "{report}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_failure_without_a_success_reports_no_data_and_success_is_retained() {
    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("keep");
    let _env = EnvGuard::acquire(&sandbox.home(), &sandbox.agent_dir());
    let deepseek = sandbox.write_stub("deepseek", "exit 1");
    sandbox.write_settings(&[("deepseek", deepseek.clone())]);
    let fixture = boot(sandbox, Vec::new()).await;

    // No success yet: the command face reports the failure and the footer
    // writes nothing.
    run_usage(&fixture, "deepseek").await;
    assert!(
        wait_until(Duration::from_secs(5), || {
            fixture
                .bridge
                .notifications()
                .iter()
                .any(|message| message.contains("no data for `deepseek`"))
        })
        .await,
        "failure report: {:?}",
        fixture.bridge.notifications()
    );
    assert_eq!(fixture.bridge.status_writes("rpi-usage"), Vec::new());

    // A successful fetch publishes the line.
    std::fs::write(
        &deepseek,
        format!("#!/bin/sh\ncat > /dev/null\n{DEEPSEEK_ENVELOPE}\n"),
    )
    .expect("fix stub");
    run_usage(&fixture, "deepseek").await;
    assert!(
        wait_until(Duration::from_secs(5), || {
            fixture.bridge.last_status("rpi-usage") == Some(Some("deepseek: CNY 9.99".to_owned()))
        })
        .await,
        "status: {:?}",
        fixture.bridge.statuses()
    );

    // Breaking the provider again keeps the last success (the host framework
    // answers the last successful envelope on failure): the footer is never
    // cleared and no flicker write is emitted.
    std::fs::write(&deepseek, "#!/bin/sh\ncat > /dev/null\nexit 1\n").expect("break stub");
    run_usage(&fixture, "deepseek").await;
    assert!(
        wait_until(Duration::from_secs(5), || {
            fixture
                .bridge
                .notifications()
                .iter()
                .filter(|message| message.contains("deepseek: CNY 9.99"))
                .count()
                >= 2
        })
        .await,
        "report after failure: {:?}",
        fixture.bridge.notifications()
    );
    assert_eq!(
        fixture.bridge.status_writes("rpi-usage"),
        vec![Some("deepseek: CNY 9.99".to_owned())],
        "no flicker write on failure"
    );
    assert_eq!(
        fixture.bridge.last_status("rpi-usage"),
        Some(Some("deepseek: CNY 9.99".to_owned()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn statusline_entry_coexists_and_the_plugin_writes_its_own_key_only() {
    use rpi_ext_host::api::ExtensionApi;
    use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};

    if plugin_path().is_none() {
        return;
    }
    let sandbox = Sandbox::new("coexist");
    let _env = EnvGuard::acquire(&sandbox.home(), &sandbox.agent_dir());
    let deepseek = sandbox.write_stub("deepseek", DEEPSEEK_ENVELOPE);
    sandbox.write_settings(&[("deepseek", deepseek)]);

    // An inline extension claims the statusline `status` key on
    // `session_start` (the statusline plugin's status placement).
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        let handler_api = api.clone();
        let _ = api.on(
            "session_start",
            Arc::new(move |_payload, _ctx| {
                let api = handler_api.clone();
                Box::pin(async move {
                    if let Some(ui) = api.runtime().ui_bridge() {
                        ui.set_status("status", Some("repo:main"));
                    }
                    Ok(Value::Null)
                })
            }),
        );
        Box::pin(async { Ok(()) })
    });
    let fixture = boot(sandbox, vec![InlineExtension::Anonymous(factory)]).await;

    fixture
        .session
        .extension_runner()
        .emit_event("model_select", json!({}))
        .await;
    assert!(
        wait_until(Duration::from_secs(5), || {
            fixture.bridge.last_status("rpi-usage") == Some(Some("deepseek: CNY 9.99".to_owned()))
                && fixture.bridge.last_status("status") == Some(Some("repo:main".to_owned()))
        })
        .await,
        "coexistence statuses: {:?}",
        fixture.bridge.statuses()
    );
    // Only the inline extension wrote `status`; the plugin wrote exactly
    // one key.
    assert_eq!(
        fixture.bridge.status_writes("status"),
        vec![Some("repo:main".to_owned())]
    );
    assert!(
        fixture
            .bridge
            .status_writes("rpi-usage")
            .iter()
            .all(|text| text.as_deref() == Some("deepseek: CNY 9.99")),
        "{:?}",
        fixture.bridge.statuses()
    );
}
