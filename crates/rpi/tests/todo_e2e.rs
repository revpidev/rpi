//! TE36 (FR-C/FR-D) — `rpiv-todo` full-chain e2e: the real cdylib through
//! the real `NativeExtensionHost` bound to real `AgentSession`s (faux
//! provider), covering the task file §2 FR-C scenario table:
//!
//! 1. `full_chain_…` — create/update/complete (faux tool calls through the
//!    real agent loop → tool-result envelopes + overlay widget lines) →
//!    fold toggle (the registered `ctrl+shift+t` shortcut handler, the
//!    same surface the interactive editor hook dispatches) → `/todos` (the
//!    real `execute_extension_command` path) → compaction survival (real
//!    manual `session.compact` → `session_compact` event → replay from the
//!    real branch) → `/reload` replay rebuild (real `session.reload()` →
//!    shutdown + host reload + `session_start` replay).
//! 2. `dual_session_isolation_…` — two hosts + two file-backed sessions
//!    (each sandbox carries its own copy of the cdylib, so the hosts get
//!    independent library statics — exactly the production parent/child
//!    shape of two processes). B's mutations never leak into A's panel or
//!    envelopes and neither lifecycle ever touches the other; B's shutdown
//!    tears down only B's overlay.
//! 3. `install_path_local_rpix_…` (FR-D) — a `.rpix` built exactly like
//!    build.yml (crate manifest + lockstep version/`minHostVersion`
//!    injection — `0.1.5-rc.1`, the first release rides the RC channel)
//!    with the cdylib under the manifest `native` name and coreutils
//!    SHA256SUMS (flat gzipped tar) goes through the real install code
//!    path (`materialize_rpix`) into the extensions root, then loads from
//!    the install directory: tool + command + shortcut + overlay all
//!    usable.
//!
//! Environment: every test holds the process-wide `ENV_LOCK` (sandboxed
//! HOME/XDG/locale/offline vars — the plugin reads
//! `$XDG_CONFIG_HOME/rpiv-todo/config.json` at install time, so a polluted
//! developer environment must not leak in; cleared locale pins the i18n
//! fallback to English), and the cdylib must exist
//! (`cargo build -p rpi-ext-todo`), otherwise the tests skip with a
//! message (l0_load / ask_user_question_e2e precedents).

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
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{json, Value};
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
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        };
        let home = home.to_string_lossy().into_owned();
        set("HOME", Some(home.clone()));
        set("USERPROFILE", Some(home));
        // The plugin config root is $XDG_CONFIG_HOME (fallback ~/.config);
        // clearing XDG keeps both inside the sandbox home (TE35 P1-2).
        set("XDG_CONFIG_HOME", None);
        set("RPI_OFFLINE", Some("1".to_owned()));
        set("RPI_SKIP_VERSION_CHECK", Some("1".to_owned()));
        // Locale → the i18n fallback (English) for deterministic chrome.
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
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
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
        let root =
            std::env::temp_dir().join(format!("rpi-todo-e2e-{tag}-{}-{nanos}", std::process::id()));
        for dir in ["cwd", "agent", "sessions", "home", "plug", "extensions"] {
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
    fn extensions_root(&self) -> PathBuf {
        self.root.join("extensions")
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
        "librpi_ext_todo.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_todo.dll"
    } else {
        "librpi_ext_todo.so"
    };
    let plugin = target.join("debug").join(name);
    if plugin.is_file() {
        Some(plugin)
    } else {
        eprintln!(
            "skipping: cdylib missing at {} — build with `cargo build -p rpi-ext-todo`",
            plugin.display()
        );
        None
    }
}

/// Install the cdylib + the crate's real manifest under the production
/// discovery root (`<agent_dir>/extensions/<name>/` — the same shape a
/// materialized `.rpix` leaves), so `load_startup_final` discovers it and
/// `host.reload()` re-discovers it (the `/reload` production path).
fn install_plugin_for_discovery(sandbox: &Sandbox, plugin: &Path) {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/rpi-ext-todo/rpi-extension.json"),
        )
        .expect("crate manifest"),
    )
    .expect("manifest json");
    let dir = sandbox.agent_dir().join("extensions").join("rpiv-todo");
    std::fs::create_dir_all(&dir).expect("extensions dir");
    // The manifest `native` name is the carrier filename on every platform
    // (the CI pack step renames the same way).
    let native = manifest["native"].as_str().expect("native").to_owned();
    std::fs::copy(plugin, dir.join(native)).expect("copy cdylib");
    std::fs::write(
        dir.join("rpi-extension.json"),
        serde_json::to_string_pretty(&manifest).expect("manifest"),
    )
    .expect("write manifest");
}

// ---------------------------------------------------------------------------
// Recording UI bridge (overlay + notify capture; no colors → plain lines)
// ---------------------------------------------------------------------------

/// One recorded `setWidget` push.
type WidgetRecord = (
    String,
    Option<WidgetContent>,
    Option<ExtensionWidgetOptions>,
);

#[derive(Default)]
struct RecordingBridge {
    widgets: Mutex<Vec<WidgetRecord>>,
    notifies: Mutex<Vec<(String, NotifyType)>>,
}

impl RecordingBridge {
    fn widget_lines(&self, key_suffix: &str) -> Option<Vec<String>> {
        let widgets = self.widgets.lock().unwrap_or_else(|e| e.into_inner());
        widgets
            .iter()
            .rev()
            .find(|(key, _, _)| key.ends_with(key_suffix))
            .and_then(|(_, content, _)| match content {
                Some(WidgetContent::Lines(lines)) => {
                    Some(lines.iter().map(|line| strip_ansi(line)).collect())
                }
                _ => None,
            })
    }

    fn widget_present(&self, key_suffix: &str) -> bool {
        let widgets = self.widgets.lock().unwrap_or_else(|e| e.into_inner());
        widgets
            .iter()
            .rev()
            .find(|(key, _, _)| key.ends_with(key_suffix))
            .is_some_and(|(_, content, _)| content.is_some())
    }

    fn push_count(&self, key_suffix: &str) -> usize {
        let widgets = self.widgets.lock().unwrap_or_else(|e| e.into_inner());
        widgets
            .iter()
            .filter(|(key, _, _)| key.ends_with(key_suffix))
            .count()
    }

    fn last_notify(&self) -> Option<(String, NotifyType)> {
        self.notifies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .cloned()
    }
}

/// Strip SGR/CSI sequences — the overlay lines carry theme ANSI (bold,
/// strikethrough); the e2e asserts on text structure (the golden frames
/// pin exact bytes since TE35).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip '[' then any parameter/intermediate bytes until a final
            // byte (0x40..=0x7E).
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[async_trait::async_trait]
impl UiBridge for RecordingBridge {
    async fn select(
        &self,
        _title: &str,
        _options: &[String],
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
    fn notify(&self, message: &str, kind: NotifyType) {
        self.notifies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((message.to_owned(), kind));
    }
    fn on_terminal_input(&self, _h: TerminalInputHandler) -> Unsubscribe {
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
            .unwrap_or_else(|e| e.into_inner())
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
        // No colors object → AnsiTheme renders unstyled text (fail-soft on
        // unknown tokens, TE35 §7.3 ruling 2); assertions strip ANSI anyway.
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
    host: Arc<NativeExtensionHost>,
    session: rpi::core::agent_session::AgentSession,
    bridge: Arc<RecordingBridge>,
    _sandbox: Sandbox,
}

/// One env sandbox per TEST (not per fixture): the dual-session test keeps
/// two fixtures alive under a single `ENV_LOCK` guard — a per-fixture
/// guard would self-deadlock on the second boot.
struct EnvHome {
    _sandbox: Sandbox,
    _guard: EnvGuard,
}

impl EnvHome {
    fn new(tag: &str) -> Self {
        let sandbox = Sandbox::new(tag);
        let _guard = EnvGuard::acquire(&sandbox.home());
        EnvHome {
            _sandbox: sandbox,
            _guard,
        }
    }
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

/// Boot a host with the real plugin + a real file-backed session bound to
/// it. `settings_json` lands in the agent dir before the resource loader
/// reads it (compaction budgets for the survival scenario).
async fn boot(
    tag: &str,
    steps: Vec<FauxResponseStep>,
    settings_json: Option<&str>,
) -> Option<Fixture> {
    let plugin = plugin_path()?;
    let sandbox = Sandbox::new(tag);
    if let Some(settings) = settings_json {
        std::fs::write(sandbox.agent_dir().join("settings.json"), settings)
            .expect("write settings.json");
    }

    install_plugin_for_discovery(&sandbox, &plugin);
    let host = Arc::new(NativeExtensionHost::new(&sandbox.cwd().to_string_lossy()));
    // The production load entry (app.rs): discovery over the agent-dir
    // extensions root, with the spec recorded so `/reload` re-runs it.
    let errors = host
        .load_startup_final(
            sandbox.agent_dir(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            false,
        )
        .await;
    assert!(errors.is_empty(), "plugin load errors: {errors:?}");
    assert!(
        host.get_tool_definition("todo").is_some(),
        "todo tool registered"
    );

    let bridge = Arc::new(RecordingBridge::default());
    host.set_ui(
        Some(bridge.clone()),
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
    // The real app binds the session-backed host actions at boot (app.rs) —
    // ctx.sessionFile / ctx.sessionToolResults need them.
    rpi::core::extension_actions::bind_session_actions(&host, &session).await;
    // The mode boot then fires session_start via bind_extensions
    // (interactive_mode.rs:4797) — the overlay's foreground claim (and its
    // ui generation) hangs off that event, so the fixture must replicate
    // the ordering: actions first, then the event.
    session
        .bind_extensions(rpi::core::agent_session::ExtensionBindings {
            mode: None,
            on_error: None,
            shutdown: None,
        })
        .await;

    Some(Fixture {
        host,
        session,
        bridge,
        _sandbox: sandbox,
    })
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

/// Run one prompt turn through the real agent loop and settle.
async fn turn(session: &rpi::core::agent_session::AgentSession, text: &str) {
    session
        .prompt(text, rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    await_idle(session, Duration::from_secs(30)).await;
}

fn todo_step(action: &str, params: Value) -> FauxResponseStep {
    let mut arguments = serde_json::Map::new();
    arguments.insert("action".to_owned(), json!(action));
    if let Value::Object(fields) = params {
        for (key, value) in fields {
            arguments.insert(key, value);
        }
    }
    faux_assistant_message(
        faux_tool_call("todo", arguments, None),
        FauxAssistantOptions::default(),
    )
    .into()
}

fn text_step(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

/// All `todo` tool results recorded in the session, oldest first.
fn todo_tool_results(session: &rpi::core::agent_session::AgentSession) -> Vec<Value> {
    session
        .messages()
        .into_iter()
        .filter_map(|message| serde_json::to_value(message).ok())
        .filter(|value| value["role"] == "toolResult" && value["toolName"] == "todo")
        .collect()
}

/// The overlay's widget key as seen by the bridge is namespaced
/// (`{extension}:rpiv-todos`, TE11 FR-E.1).
const WIDGET_KEY: &str = ":rpiv-todos";

// ---------------------------------------------------------------------------
// FR-C scenario 1 — full chain: create/update/complete → collapse →
// /todos → compaction survival → /reload replay rebuild
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_chain_create_update_complete_collapse_todos_compact_reload() {
    // Small keepRecentTokens so the manual compaction actually summarizes
    // (rpc_mode_test compact_command precedent); #9740 keeps the trailing
    // tool-call/result pair (the last todo snapshot) across the cut.
    if plugin_path().is_none() {
        return;
    }
    let _env = EnvHome::new("full");
    let Some(f) = boot(
        "full",
        vec![
            // A turn keeps streaming while the model returns tool calls, so
            // each turn is one tool step + one text step that ends it.
            todo_step("create", json!({"subject": "Design the API"})),
            text_step("Tracked."),
            todo_step(
                "update",
                json!({"id": 1, "status": "in_progress", "activeForm": "designing the API"}),
            ),
            text_step("Started."),
            todo_step("create", json!({"subject": "Write tests"})),
            text_step("Added."),
            todo_step("update", json!({"id": 1, "status": "completed"})),
            text_step("Done."),
            // Manual compaction can make up to two summarization calls when
            // the cut lands mid-turn (#9740 turn-prefix machinery); a tool
            // step served to a summarization fails with "attempted to call
            // a tool", so pad text steps.
            text_step("Turn prefix summary."),
            // The compaction summary request consumes this step.
            text_step("Summary of the conversation so far."),
            // One more turn after compaction (post-compact sanity).
            todo_step("list", json!({})),
            text_step("Listed."),
        ],
        Some(r#"{"compaction": {"keepRecentTokens": 5}}"#),
    )
    .await
    else {
        return;
    };
    // --- create ------------------------------------------------------------
    turn(&f.session, "track: design the API").await;
    let results = todo_tool_results(&f.session);
    assert_eq!(results.len(), 1, "one todo tool result: {results:?}");
    assert_eq!(
        results[0]["details"]["tasks"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        results[0]["details"]["tasks"][0]["subject"],
        "Design the API"
    );
    assert_eq!(results[0]["details"]["nextId"], 2);
    let lines = f
        .bridge
        .widget_lines(WIDGET_KEY)
        .expect("overlay registered");
    assert!(lines[0].contains("Todos (0/1)"), "heading: {lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("○ Design the API")),
        "{lines:?}"
    );
    assert_eq!(lines.last(), Some(&String::new()), "spacer: {lines:?}");

    // --- update (in_progress + activeForm annotation) ---------------------
    turn(&f.session, "start designing").await;
    let lines = f
        .bridge
        .widget_lines(WIDGET_KEY)
        .expect("overlay after update");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("◐ Design the API") && l.contains("(designing the API)")),
        "{lines:?}"
    );

    // --- second task + complete --------------------------------------------
    turn(&f.session, "add the test task").await;
    turn(&f.session, "api is done").await;
    let lines = f
        .bridge
        .widget_lines(WIDGET_KEY)
        .expect("overlay after complete");
    assert!(lines[0].contains("Todos (1/2)"), "heading: {lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("✓ Design the API")),
        "completed row: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("○ Write tests")),
        "pending row: {lines:?}"
    );
    let results = todo_tool_results(&f.session);
    assert_eq!(results.len(), 4, "four todo results: {results:?}");
    assert_eq!(
        results[3]["details"]["tasks"][0]["status"], "completed",
        "envelope snapshot carries the completed task"
    );

    // --- fold toggle (the registered shortcut handler; same entry as the
    //     editor hook) ----------------------------------------------------
    let shortcuts = f.host.get_shortcuts(&[]);
    let collapse = shortcuts
        .iter()
        .find(|(key, _)| *key == "ctrl+shift+t")
        .map(|(_, shortcut)| shortcut.handler.clone())
        .expect("ctrl+shift+t shortcut registered");
    collapse(f.host.core().create_context())
        .await
        .expect("collapse");
    let lines = f.bridge.widget_lines(WIDGET_KEY).expect("collapsed form");
    assert_eq!(
        lines.len(),
        3,
        "collapsed = title + hint + spacer, got {lines:?}"
    );
    assert!(lines[0].contains("Todos (1/2)"), "{lines:?}");
    assert!(
        lines[1].contains("ctrl+shift+t to expand"),
        "hint shows the configured key: {lines:?}"
    );
    collapse(f.host.core().create_context())
        .await
        .expect("expand");
    let lines = f.bridge.widget_lines(WIDGET_KEY).expect("expanded again");
    assert!(lines.len() > 2, "expanded rows return: {lines:?}");

    // --- /todos command (the real execute_extension_command path) ---------
    let executed = f
        .session
        .extension_runner()
        .execute_extension_command("todos", "")
        .await;
    assert!(executed, "the todos command resolves to the plugin");
    let (message, kind) = f.bridge.last_notify().expect("notify output");
    assert_eq!(kind, NotifyType::Info, "grouped listing is info-level");
    assert!(
        message.contains("1/2 completed") && message.contains("1 pending"),
        "counts header: {message}"
    );
    assert!(
        message.contains("Design the API") && message.contains("Write tests"),
        "grouped rows: {message}"
    );

    // --- compaction survival (real manual compact -> session_compact ->
    //     replay) ---------------------------------------------------------
    let before = f
        .bridge
        .widget_lines(WIDGET_KEY)
        .expect("overlay before compact");
    let compacted = f.session.compact(None).await.expect("compaction");
    assert!(
        compacted.summary.contains("Summary of the conversation"),
        "the faux summary ran: {:?}",
        compacted.summary
    );
    // session_compact → replay from the real branch. #9740 keeps the
    // trailing todo tool-result pair across the cut, so the last full
    // snapshot (2 tasks, 1 completed) survives and re-renders.
    let after = f
        .bridge
        .widget_lines(WIDGET_KEY)
        .expect("overlay after compact");
    assert!(
        after.iter().any(|l| l.contains("✓ Design the API")),
        "completed task survives compaction: {after:?}"
    );
    assert!(
        after.iter().any(|l| l.contains("○ Write tests")),
        "pending task survives compaction: {after:?}"
    );
    assert_eq!(
        strip_heading(&before[0]),
        strip_heading(&after[0]),
        "heading unchanged across compaction"
    );

    // --- /reload replay rebuild (real reload: shutdown -> host reload ->
    //     start) ----------------------------------------------------------
    f.session.reload().await;
    let rebuilt = f
        .bridge
        .widget_lines(WIDGET_KEY)
        .expect("overlay after reload");
    assert!(
        rebuilt.iter().any(|l| l.contains("✓ Design the API")),
        "completed task rebuilt from the branch after /reload: {rebuilt:?}"
    );
    assert!(
        rebuilt.iter().any(|l| l.contains("○ Write tests")),
        "pending task rebuilt after /reload: {rebuilt:?}"
    );

    // --- still usable after compaction (list action round-trips) ----------
    turn(&f.session, "list the tasks again").await;
    // Compaction summarized the earlier turns away — the branch keeps the
    // tail only, so exactly one todo result (this list call) remains,
    // carrying the full snapshot rebuilt from the surviving store slot.
    let results = todo_tool_results(&f.session);
    assert_eq!(results.len(), 1, "post-compact list ran: {results:?}");
    assert_eq!(
        results[0]["details"]["tasks"].as_array().map(Vec::len),
        Some(2),
        "list envelope carries both tasks after compaction"
    );
}

fn strip_heading(heading: &str) -> String {
    // Compare headings without the accent/dim glyph variance.
    heading.replace(['●', '○'], "o")
}
// ---------------------------------------------------------------------------
// FR-C scenario 2 — dual-session isolation (two hosts, shared statics)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dual_session_isolation_store_slots_and_foreground_gate() {
    if plugin_path().is_none() {
        return;
    }
    let _env = EnvHome::new("iso");
    let (Some(a), Some(b)) = (
        boot(
            "iso-a",
            vec![
                todo_step("create", json!({"subject": "Parent task"})),
                text_step("Tracked."),
            ],
            None,
        )
        .await,
        boot(
            "iso-b",
            vec![
                todo_step("create", json!({"subject": "Child task"})),
                text_step("Tracked."),
            ],
            None,
        )
        .await,
    ) else {
        return;
    };

    // A claims the shared foreground (first UI-bearing session_start).
    turn(&a.session, "parent: track your task").await;
    let a_lines = a.bridge.widget_lines(WIDGET_KEY).expect("A overlay");
    assert!(
        a_lines.iter().any(|l| l.contains("Parent task")),
        "{a_lines:?}"
    );
    let a_pushes = a.bridge.push_count(WIDGET_KEY);

    // B (own host, own session file, same process → shared cdylib statics):
    // its tool call lands in B's OWN store slot.
    turn(&b.session, "child: track your own task").await;
    let b_results = todo_tool_results(&b.session);
    assert_eq!(b_results.len(), 1);
    let b_tasks = b_results[0]["details"]["tasks"].as_array().expect("tasks");
    assert_eq!(b_tasks.len(), 1, "B's envelope: {b_tasks:?}");
    assert_eq!(b_tasks[0]["subject"], "Child task");
    assert!(
        !b_tasks.iter().any(|t| t["subject"] == "Parent task"),
        "A's task never leaks into B's envelope"
    );

    // A's panel is untouched by B's mutation.
    let a_lines = a
        .bridge
        .widget_lines(WIDGET_KEY)
        .expect("A overlay still there");
    assert!(
        a_lines.iter().any(|l| l.contains("Parent task"))
            && !a_lines.iter().any(|l| l.contains("Child task")),
        "A's panel shows only A's task: {a_lines:?}"
    );

    // Each sandbox carries its own copy of the cdylib, so the two hosts
    // get independent library statics — exactly the production parent /
    // child shape (two processes). B is the foreground of ITS OWN store:
    // its panel shows only B's tasks, and neither panel ever mixes.
    let b_lines = b.bridge.widget_lines(WIDGET_KEY).expect("B's own overlay");
    assert!(
        b_lines.iter().any(|l| l.contains("Child task"))
            && !b_lines.iter().any(|l| l.contains("Parent task")),
        "B's panel shows only B's tasks: {b_lines:?}"
    );

    // B's /todos reads B's own slot (notify through B's host).
    let executed = b
        .session
        .extension_runner()
        .execute_extension_command("todos", "")
        .await;
    assert!(executed);
    let (message, _) = b.bridge.last_notify().expect("B's /todos output");
    assert!(
        message.contains("Child task") && !message.contains("Parent task"),
        "B's listing shows only B's tasks: {message}"
    );

    // B's shutdown evicts only B (sid-gated teardown): A's panel survives.
    b.session
        .extension_runner()
        .emit_event(
            "session_shutdown",
            json!({"type": "session_shutdown", "reason": "shutdown"}),
        )
        .await;
    // B's shutdown tears down B's own overlay (its last widget push is the
    // removal) and never touches A's.
    assert!(
        !b.bridge.widget_present(WIDGET_KEY),
        "B's overlay is torn down by B's shutdown"
    );
    assert!(
        a.bridge.widget_present(WIDGET_KEY),
        "A's overlay survives B's shutdown"
    );
    let a_lines = a
        .bridge
        .widget_lines(WIDGET_KEY)
        .expect("A overlay after B shutdown");
    assert!(
        a_lines.iter().any(|l| l.contains("Parent task")),
        "{a_lines:?}"
    );
    assert!(
        a.bridge.push_count(WIDGET_KEY) >= a_pushes,
        "A's pushes never decreased"
    );

    // A's own /todos still sees its task after B is gone.
    let executed = a
        .session
        .extension_runner()
        .execute_extension_command("todos", "")
        .await;
    assert!(executed);
    let (message, _) = a.bridge.last_notify().expect("A's /todos output");
    assert!(
        message.contains("Parent task") && !message.contains("Child task"),
        "A's listing after B's shutdown: {message}"
    );
}

// ---------------------------------------------------------------------------
// FR-D — install path: local .rpix → materialize_rpix → load → three
// surfaces usable
// ---------------------------------------------------------------------------

/// Build a `.rpix` exactly like build.yml's pack step: the crate manifest
/// with the lockstep fields injected (version = minHostVersion = the
/// release version — `0.1.5-rc.1`, the RC-channel first release), the
/// cdylib under the manifest `native` filename, coreutils SHA256SUMS,
/// gzipped tar with a flat root (design §3.1).
fn build_local_rpix(manifest: &Value, cdylib: &[u8], version: &str) -> Vec<u8> {
    let native = manifest["native"].as_str().expect("native name");
    let injected = serde_json::to_vec(&json!({
        "name": manifest["name"],
        "version": version,
        "minHostVersion": version,
        "description": manifest["description"],
        "native": native,
        "capabilities": manifest["capabilities"],
        "rpiAbi": manifest["rpiAbi"],
    }))
    .expect("injected manifest");

    let mut all: Vec<(String, Vec<u8>)> = vec![
        ("rpi-extension.json".to_owned(), injected),
        (native.to_owned(), cdylib.to_vec()),
    ];
    let sums = all
        .iter()
        .map(|(path, content)| format!("{}  {path}\n", rpi::core::self_update::sha256_hex(content)))
        .collect::<String>();
    all.push(("SHA256SUMS".to_owned(), sums.into_bytes()));

    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, content) in &all {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, &content[..])
            .expect("tar append");
    }
    builder
        .into_inner()
        .expect("tar finish")
        .finish()
        .expect("gz finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn install_path_local_rpix_loads_tool_command_and_overlay() {
    let Some(plugin) = plugin_path() else { return };
    let sandbox = Sandbox::new("install");
    let _env = EnvGuard::acquire(&sandbox.home());

    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/rpi-ext-todo/rpi-extension.json"),
        )
        .expect("crate manifest"),
    )
    .expect("manifest json");
    let cdylib = std::fs::read(&plugin).expect("read cdylib");
    // The lockstep injection the CI performs at pack time; the first
    // release rides the RC channel (v0.1.5-rc.1, V14-19).
    let archive = build_local_rpix(&manifest, &cdylib, "0.1.5-rc.1");

    // The real install code path (registry channel materialization).
    let install_dir = rpi::core::extension_registry::materialize_rpix(
        &archive,
        &sandbox.extensions_root(),
        "rpiv-todo",
        "0.1.5-rc.1",
    )
    .expect("materialize rpix");
    let installed: Value = serde_json::from_str(
        &std::fs::read_to_string(install_dir.join("rpi-extension.json"))
            .expect("installed manifest"),
    )
    .expect("installed manifest json");
    assert_eq!(installed["version"], "0.1.5-rc.1");
    assert_eq!(
        installed["minHostVersion"], "0.1.5-rc.1",
        "lockstep injection lands in the packed manifest"
    );
    assert!(
        install_dir
            .join(manifest["native"].as_str().expect("native"))
            .is_file(),
        "cdylib landed under the manifest native name"
    );

    // Load from the install directory (the loader's local discovery shape).
    let host = Arc::new(NativeExtensionHost::new(&sandbox.cwd().to_string_lossy()));
    let errors = host.load_paths(&[install_dir]).await;
    assert!(errors.is_empty(), "install-dir load errors: {errors:?}");

    // Surface 1 — the tool; surface 3's driver — the overlay.
    assert!(host.get_tool_definition("todo").is_some(), "tool surface");
    // Surface 2 — the /todos command.
    assert!(host.get_command("todos").is_some(), "command surface");
    // The collapse shortcut (the overlay's third control plane).
    let shortcuts = host.get_shortcuts(&[]);
    assert!(
        shortcuts
            .iter()
            .any(|(key, _)| key.as_str() == "ctrl+shift+t"),
        "shortcut surface"
    );

    // Drive one real tool call through a session bound to this host; the
    // overlay must register on the bridge (all three surfaces usable
    // end-to-end).
    let bridge = Arc::new(RecordingBridge::default());
    host.set_ui(
        Some(bridge.clone()),
        rpi_ext_host::types::ExtensionMode::Tui,
    );
    let (model_runtime, model) = build_model_runtime(
        &sandbox.agent_dir(),
        vec![
            todo_step("create", json!({"subject": "Installed from rpix"})),
            text_step("Tracked."),
        ],
    )
    .await;
    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: sandbox.cwd(),
            agent_dir: Some(sandbox.agent_dir()),
            settings_manager: None,
            model_runtime: Some(model_runtime),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(sandbox.cwd()),
        agent_dir: Some(sandbox.agent_dir()),
        model_runtime: None,
        model: Some(model),
        services: Some(services),
        session_manager: Some(Arc::new(Mutex::new(
            rpi::core::session_manager::SessionManager::create(
                &sandbox.cwd(),
                Some(&sandbox.sessions()),
                rpi::core::session_manager::NewSessionOptions::default(),
            )
            .expect("file-backed session"),
        ))),
        extension_host: Some(host.clone()),
        ..Default::default()
    })
    .await
    .expect("create session");
    let session = created.session;
    rpi::core::extension_actions::bind_session_actions(&host, &session).await;
    // Same production ordering as boot(): session_start fires at mode boot
    // (bind_extensions), after the actions bind.
    session
        .bind_extensions(rpi::core::agent_session::ExtensionBindings {
            mode: None,
            on_error: None,
            shutdown: None,
        })
        .await;

    turn(&session, "use the installed todo tool").await;
    let results = todo_tool_results(&session);
    assert_eq!(results.len(), 1, "tool executed from the installed .rpix");
    let lines = bridge
        .widget_lines(WIDGET_KEY)
        .expect("overlay from installed plugin");
    assert!(
        lines.iter().any(|l| l.contains("Installed from rpix")),
        "{lines:?}"
    );
}

// ---------------------------------------------------------------------------
// Regression (v0.1.5-rc.1 field finding): the REAL mode boot order —
// `set_ui` must attach the UI bridge BEFORE `bind_extensions` fires
// `session_start`, or the overlay's foreground claim never runs. This test
// boots the actual `InteractiveMode` (whose `init` performs both steps in
// the production order) and asserts the overlay lands on the screen.
// ---------------------------------------------------------------------------

struct ModeTerm {
    writes: Arc<Mutex<String>>,
    input_handler: Arc<Mutex<Option<rpi_tui::terminal::InputHandler>>>,
}

impl ModeTerm {
    fn new() -> Self {
        ModeTerm {
            writes: Arc::new(Mutex::new(String::new())),
            input_handler: Arc::new(Mutex::new(None)),
        }
    }

    fn screen(&self) -> String {
        rpi_test_support::vt::strip_ansi(
            &self
                .writes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        )
    }
}

impl Clone for ModeTerm {
    fn clone(&self) -> Self {
        ModeTerm {
            writes: Arc::clone(&self.writes),
            input_handler: Arc::clone(&self.input_handler),
        }
    }
}

impl rpi_tui::terminal::Terminal for ModeTerm {
    fn start(
        &mut self,
        on_input: rpi_tui::terminal::InputHandler,
        _on_resize: rpi_tui::terminal::ResizeHandler,
    ) {
        *self
            .input_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(on_input);
    }

    fn stop(&mut self) {
        *self
            .input_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    fn drain_input(
        &mut self,
        _max_ms: Option<u64>,
        _idle_ms: Option<u64>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }

    fn write(&mut self, data: &str) {
        self.writes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_str(data);
    }

    fn columns(&self) -> u16 {
        100
    }

    fn rows(&self) -> u16 {
        30
    }

    fn kitty_protocol_active(&self) -> bool {
        false
    }

    fn move_by(&mut self, _lines: i32) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
    fn set_title(&mut self, _title: &str) {}
    fn set_progress(&mut self, _active: bool) {}
    fn pump(&mut self, _timeout: Option<Duration>) -> bool {
        std::thread::sleep(Duration::from_millis(2));
        false
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_mode_boot_order_shows_the_todo_overlay() {
    if plugin_path().is_none() {
        return;
    }
    let _env = EnvHome::new("mode");
    let sandbox = Sandbox::new("mode");
    let Some(plugin) = plugin_path() else {
        return;
    };
    install_plugin_for_discovery(&sandbox, &plugin);

    let host = Arc::new(NativeExtensionHost::new(&sandbox.cwd().to_string_lossy()));
    let load_errors = host
        .load_startup_final(
            sandbox.agent_dir(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            false,
        )
        .await;
    assert!(
        load_errors.is_empty(),
        "plugin load errors: {load_errors:?}"
    );

    // Raw session: NO set_ui and NO bind_extensions here — the real mode's
    // init() owns both, in the production order under test.
    let (model_runtime, model) = build_model_runtime(
        &sandbox.agent_dir(),
        vec![
            todo_step("create", json!({"subject": "Boot order task"})),
            text_step("Tracked."),
        ],
    )
    .await;
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
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(sandbox.cwd()),
        agent_dir: Some(sandbox.agent_dir()),
        model_runtime: None,
        model: Some(model),
        services: Some(services.clone()),
        session_manager: Some(Arc::new(Mutex::new(
            rpi::core::session_manager::SessionManager::create(
                &sandbox.cwd(),
                Some(&sandbox.sessions()),
                rpi::core::session_manager::NewSessionOptions::default(),
            )
            .expect("file-backed session"),
        ))),
        extension_host: Some(host.clone()),
        ..Default::default()
    })
    .await
    .expect("create session");
    rpi::core::extension_actions::bind_session_actions(&host, &created.session).await;

    let factory: rpi::core::agent_session_runtime::CreateAgentSessionRuntimeFactory = Arc::new(
        |_options: rpi::core::agent_session_runtime::CreateRuntimeOptions| {
            Box::pin(async { unreachable!("session replacement is not exercised here") })
        },
    );
    let runtime = rpi::core::agent_session_runtime::AgentSessionRuntime::new(
        created.session,
        services,
        factory,
        Vec::new(),
        None,
    );
    let term = ModeTerm::new();
    let mut mode = rpi::modes::interactive::interactive_mode::InteractiveMode::with_terminal(
        runtime,
        rpi::modes::interactive::interactive_mode::InteractiveModeOptions {
            initial_message: Some("go".to_owned()),
            ..Default::default()
        },
        Box::new(ModeTerm::clone(&term)),
    );
    let shutdown = mode.shutdown_sender();
    // `InteractiveMode` is not `Send` (the unsubscribe `Box<dyn FnOnce()>`);
    // run the real mode loop on a dedicated thread with its own
    // current-thread runtime (ask_user_question_e2e precedent).
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let run_thread = std::thread::Builder::new()
        .name("todo-e2e-mode".to_string())
        .spawn(move || {
            let thread_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("mode runtime");
            thread_runtime.block_on(async move {
                mode.run().await;
            });
            let _ = done_tx.send(());
        })
        .expect("spawn mode thread");

    // The overlay must reach the screen: the mode's init() attaches the UI
    // bridge and THEN fires session_start (the rc.1 bug inverted this, so
    // the foreground claim — and with it every widget push — never ran).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let screen = term.screen();
        if screen.contains("Todos (0/1)") && screen.contains("Boot order task") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "overlay never reached the screen; last screen:\n{}",
            term.screen()
        );
        sleep(Duration::from_millis(50)).await;
    }

    let _ = shutdown.send(true);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if done_rx.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "mode.run() did not exit");
        sleep(Duration::from_millis(25)).await;
    }
    let _ = run_thread.join();
}
