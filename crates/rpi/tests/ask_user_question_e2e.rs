//! V14-24 (C4) pilot e2e — `rpiv-ask-user-question` through the real rpi
//! stacks (task `rpi-docs/plan/v0.1.4/V14-24-interactive-ui-abi-c4-pilot.md`).
//!
//! The ABI's acceptance moves from fixture parity (C1/C2) to full-stack
//! integration: the real interactive mode (in-memory terminal + real
//! `AgentSession` + faux provider) and the real RPC mode drive the actual
//! plugin cdylib over the real host-call chain:
//!
//! - TUI pilot (§4.1): faux tool call → dialog mounts as an overlay →
//!   scripted key bytes through the real input pipeline → tool-result
//!   envelope + session JSONL + `rpiv:ask-user:*` bus events (payload and
//!   bracket timing, R-Q4.1/R-Q4.2).
//! - RPC / fallback (§4.2): `ctx.mode == "rpc"` routes to the dialog walker
//!   over the real `extension_ui_request` frame loop; a legacy bridge (no
//!   interactive UI) answers `unknownMethod` on `ui.mountComponent` and
//!   falls back to the same walker with an identical envelope (R-Q6.1/
//!   R-Q6.2); a bridge-less host hides the tool from the model entirely via
//!   the `before_agent_start` reconciler (R-Q6.3).
//!
//! The pilot found one real integration gap, fixed with this task: the
//! plugin's `events.emit` calls used `{"event","payload"}` args while the
//! host dispatch reads the ABI wire form `{"channel","data"}` — the
//! `rpiv:ask-user:*` events never reached their channels on a real host
//! (see `rpi-ext-ask-user-question/src/events.rs`).
//!
//! Environment: every test holds the process-wide `ENV_LOCK` (sandboxed
//! HOME/XDG/locale/offline vars), so the tests serialize within this binary;
//! the cdylib must exist (`cargo build -p rpi-ext-ask-user-question`),
//! otherwise the tests skip with a message (l0_load precedent).

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rpi_ext_host::api::{
    ExtensionWidgetOptions, NotifyType, SetThemeResult, TerminalInputHandler, ThemeInfo, UiBridge,
    UiDialogOptions, Unsubscribe, WidgetContent, WorkingIndicatorOptions,
};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use rpi_tui::terminal::{InputHandler, ResizeHandler, Terminal};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

// ---------------------------------------------------------------------------
// Shared constants (envelope parity between the TUI and walker paths)
// ---------------------------------------------------------------------------

/// Envelope text when the single question is answered with the "Alpha"
/// option — asserted identically on the TUI path and the RPC/legacy walker
/// paths (R-Q6.1 "结果信封与 TUI 路径一致").
const ALPHA_ENVELOPE: &str = concat!(
    "User has answered your questions: \"Pick one?\"=\"Alpha\". ",
    "You can now continue with the user's answers in mind."
);
/// Envelope text for a declined questionnaire (upstream `DECLINE_MESSAGE`).
const DECLINE_ENVELOPE: &str = "User declined to answer questions";

/// Single question with the "Alpha"/"Beta" options (drives
/// [`ALPHA_ENVELOPE`]).
fn pick_one_params() -> Value {
    json!({
        "questions": [{
            "question": "Pick one?",
            "header": "Pick",
            "options": [
                {"label": "Alpha", "description": "alpha option"},
                {"label": "Beta", "description": "beta option"}
            ]
        }]
    })
}

fn tool_call_step(params: &Value) -> FauxResponseStep {
    faux_assistant_message(
        faux_tool_call(
            "ask_user_question",
            params.as_object().cloned().unwrap_or_default(),
            None,
        ),
        FauxAssistantOptions::default(),
    )
    .into()
}

fn text_step(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

// ---------------------------------------------------------------------------
// Environment sandbox (process-wide env mutations → tests serialize)
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Env snapshot + restore guard; sets the pilot sandbox environment.
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
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("rpi-askq-e2e-{tag}-{}-{nanos}", std::process::id()));
        for dir in ["cwd", "agent", "sessions", "home", "plug"] {
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
        "librpi_ext_ask_user_question.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_ask_user_question.dll"
    } else {
        "librpi_ext_ask_user_question.so"
    };
    let plugin = target.join("debug").join(name);
    if plugin.is_file() {
        Some(plugin)
    } else {
        eprintln!(
            "skipping: cdylib missing at {} — build with `cargo build -p rpi-ext-ask-user-question`",
            plugin.display()
        );
        None
    }
}

/// Copy the built cdylib + the crate's real manifest into the sandbox.
fn package_plugin(sandbox: &Sandbox, plugin: &Path) -> PathBuf {
    let dir = sandbox.root.join("plug");
    std::fs::copy(plugin, dir.join(plugin.file_name().expect("plugin name"))).expect("copy cdylib");
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rpi-ext-ask-user-question/rpi-extension.json"),
    )
    .expect("read crate manifest");
    std::fs::write(dir.join("rpi-extension.json"), manifest).expect("write manifest");
    dir
}

async fn load_host(plugin_dir: &Path, cwd: &Path) -> Arc<NativeExtensionHost> {
    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let errors = host
        .load_paths(std::slice::from_ref(&plugin_dir.to_path_buf()))
        .await;
    assert!(errors.is_empty(), "plugin load errors: {errors:?}");
    assert!(
        host.get_tool_definition("ask_user_question").is_some(),
        "ask_user_question registered"
    );
    host
}

// ---------------------------------------------------------------------------
// In-memory terminal (TestTerminal shape, public-API edition)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Term {
    writes: Arc<Mutex<String>>,
    input_handler: Arc<Mutex<Option<InputHandler>>>,
    resize_handler: Arc<Mutex<Option<ResizeHandler>>>,
}

impl Term {
    fn new() -> Self {
        Term {
            writes: Arc::new(Mutex::new(String::new())),
            input_handler: Arc::new(Mutex::new(None)),
            resize_handler: Arc::new(Mutex::new(None)),
        }
    }

    /// Feed raw input as if typed (upstream `process.stdin.emit("data")`).
    fn feed(&self, data: &str) {
        let mut handler = self
            .input_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(handler) = handler.as_mut() {
            handler(data);
        }
    }

    /// All bytes written so far, ANSI-stripped.
    fn screen(&self) -> String {
        rpi_test_support::vt::strip_ansi(
            &self
                .writes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        )
    }

    fn occurrences(&self, needle: &str) -> usize {
        self.screen().matches(needle).count()
    }
}

impl Terminal for Term {
    fn start(&mut self, on_input: InputHandler, on_resize: ResizeHandler) {
        *self
            .input_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(on_input);
        *self
            .resize_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(on_resize);
    }

    fn stop(&mut self) {
        *self
            .input_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        *self
            .resize_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    fn drain_input(
        &mut self,
        _max_ms: Option<u64>,
        _idle_ms: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }

    fn write(&mut self, data: &str) {
        self.writes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_str(data);
    }

    fn columns(&self) -> u16 {
        80
    }

    fn rows(&self) -> u16 {
        24
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

// ---------------------------------------------------------------------------
// Waiting helpers
// ---------------------------------------------------------------------------

/// Poll until the predicate over the ANSI-stripped writes holds.
async fn await_screen(term: &Term, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if term.screen().contains(needle) {
            return;
        }
        sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "terminal never showed {needle:?}; last screen:\n{}",
        term.screen()
    );
}

/// Poll until the session is idle.
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

/// The ask_user_question tool-result message from the session, if any yet.
fn askq_tool_result(session: &rpi::core::agent_session::AgentSession) -> Option<Value> {
    session
        .messages()
        .into_iter()
        .filter_map(|message| serde_json::to_value(message).ok())
        .find(|value| value["role"] == "toolResult" && value["toolName"] == "ask_user_question")
}

fn assert_alpha_envelope(result: &Value) {
    assert_eq!(result["content"][0]["text"], ALPHA_ENVELOPE);
    assert_eq!(result["details"]["cancelled"], json!(false));
    assert_eq!(result["details"]["answers"][0]["kind"], json!("option"));
    assert_eq!(result["details"]["answers"][0]["answer"], json!("Alpha"));
}

// ---------------------------------------------------------------------------
// Bus recorder (rpiv:ask-user:* events through the real host event bus)
// ---------------------------------------------------------------------------

type BusTimeline = Arc<Mutex<Vec<(&'static str, Value)>>>;

fn record_bus(host: &NativeExtensionHost) -> BusTimeline {
    let timeline: BusTimeline = Arc::new(Mutex::new(Vec::new()));
    for channel in ["rpiv:ask-user:prompt", "rpiv:ask-user:blocked"] {
        let sink = timeline.clone();
        let unsubscribe = host.event_bus().on(
            channel,
            Arc::new(move |data| {
                let name = match channel {
                    "rpiv:ask-user:prompt" => "prompt",
                    _ => "blocked",
                };
                sink.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push((name, data));
            }),
        );
        // Keep the subscription alive for the host's lifetime — the
        // recorder never unsubscribes.
        std::mem::forget(unsubscribe);
    }
    timeline
}

// ---------------------------------------------------------------------------
// Session/runtime builders (real services + faux provider + plugin host)
// ---------------------------------------------------------------------------

async fn faux_model_runtime(
    agent_dir: &Path,
    responses: Vec<FauxResponseStep>,
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
        .register_native_provider(Arc::new(FauxAiProvider::new(provider)))
        .await
        .expect("register faux provider");
    (model_runtime, model)
}

async fn build_session(
    sandbox: &Sandbox,
    host: Arc<NativeExtensionHost>,
    model_runtime: Arc<rpi::core::model_runtime::ModelRuntime>,
    model: rpi_ai::types::Model,
    persist: bool,
) -> rpi::core::agent_session::AgentSession {
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
    let session_manager = if persist {
        rpi::core::session_manager::SessionManager::create(
            &sandbox.cwd(),
            Some(&sandbox.sessions()),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("file-backed session")
    } else {
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&sandbox.cwd()),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session")
    };
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(sandbox.cwd()),
        agent_dir: Some(sandbox.agent_dir()),
        model_runtime: None,
        model: Some(model),
        services: Some(services.clone()),
        session_manager: Some(Arc::new(Mutex::new(session_manager))),
        extension_host: Some(host),
        ..Default::default()
    })
    .await
    .expect("create session");
    created.session
}

// ---------------------------------------------------------------------------
// TUI fixture: real InteractiveMode over the plugin session
// ---------------------------------------------------------------------------

struct TuiFixture {
    term: Term,
    session: rpi::core::agent_session::AgentSession,
    bus: BusTimeline,
    _sandbox: Sandbox,
    _env: EnvGuard,
    shutdown: tokio::sync::watch::Sender<bool>,
    done_rx: std::sync::mpsc::Receiver<()>,
    run_thread: Option<std::thread::JoinHandle<()>>,
}

impl TuiFixture {
    /// Boot the real interactive mode with the initial prompt; returns once
    /// the dialog question is on screen (or panics with the screen dump).
    async fn boot(tag: &str, params: &Value) -> Option<Self> {
        let plugin = plugin_path()?;
        let sandbox = Sandbox::new(tag);
        let env = EnvGuard::acquire(&sandbox.home());
        let plugin_dir = package_plugin(&sandbox, &plugin);
        let host = load_host(&plugin_dir, &sandbox.cwd()).await;
        let bus = record_bus(&host);
        let (model_runtime, model) = faux_model_runtime(
            &sandbox.agent_dir(),
            vec![tool_call_step(params), text_step("all done")],
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
        let session_manager = Arc::new(Mutex::new(
            rpi::core::session_manager::SessionManager::create(
                &sandbox.cwd(),
                Some(&sandbox.sessions()),
                rpi::core::session_manager::NewSessionOptions::default(),
            )
            .expect("file-backed session"),
        ));
        let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
            cwd: Some(sandbox.cwd()),
            agent_dir: Some(sandbox.agent_dir()),
            model_runtime: None,
            model: Some(model),
            services: Some(services.clone()),
            session_manager: Some(session_manager),
            extension_host: Some(host.clone()),
            ..Default::default()
        })
        .await
        .expect("create session");
        let session = created.session;
        // The real app binds the session-backed host actions at boot
        // (app.rs) — the reconciler and tool-list management need them.
        rpi::core::extension_actions::bind_session_actions(&host, &session).await;

        let factory: rpi::core::agent_session_runtime::CreateAgentSessionRuntimeFactory = Arc::new(
            |_options: rpi::core::agent_session_runtime::CreateRuntimeOptions| {
                Box::pin(async {
                    unreachable!("session creation is not exercised by the pilot e2e")
                })
            },
        );
        let runtime = rpi::core::agent_session_runtime::AgentSessionRuntime::new(
            session.clone(),
            services,
            factory,
            Vec::new(),
            None,
        );
        let term = Term::new();
        let mut mode = rpi::modes::interactive::interactive_mode::InteractiveMode::with_terminal(
            runtime,
            rpi::modes::interactive::interactive_mode::InteractiveModeOptions {
                initial_message: Some("go".to_owned()),
                ..Default::default()
            },
            Box::new(Term::clone(&term)),
        );
        let shutdown = mode.shutdown_sender();
        // `InteractiveMode` carries a `Box<dyn FnOnce()>` (the session-event
        // unsubscribe), which is not `Send` — `mode.run()` cannot be
        // `tokio::spawn`ed. Run the real mode loop on a dedicated thread
        // with its own current-thread runtime (the driver thread it spawns
        // keeps pumping input/render independently).
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let run_thread = std::thread::Builder::new()
            .name("pilot-e2e-mode".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("mode runtime");
                runtime.block_on(async move {
                    mode.run().await;
                });
                let _ = done_tx.send(());
            })
            .expect("spawn mode thread");

        let fixture = TuiFixture {
            term,
            session,
            bus,
            _sandbox: sandbox,
            _env: env,
            shutdown,
            done_rx,
            run_thread: Some(run_thread),
        };
        // The dialog frame carries the en footer hint ("Enter to select") —
        // unique to the mounted overlay (the chat tool-call bubble renders
        // the arguments, never the footer hints).
        await_screen(&fixture.term, "Enter to select", Duration::from_secs(20)).await;
        Some(fixture)
    }

    /// Wait out the turn, snapshot the tool result + bus timeline, and shut
    /// the mode down (borrowing so the sandbox outlives the caller's
    /// file-backed assertions — `Sandbox::drop` removes the temp tree).
    async fn finish(&mut self) -> (Value, Vec<(String, Value)>) {
        await_idle(&self.session, Duration::from_secs(30)).await;
        let result = askq_tool_result(&self.session).expect("ask_user_question tool result");
        let bus = self
            .bus
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|(name, data)| ((*name).to_owned(), data.clone()))
            .collect::<Vec<_>>();
        self.shutdown.send(true).expect("send shutdown");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.done_rx.try_recv().is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "mode.run() did not exit");
            sleep(Duration::from_millis(25)).await;
        }
        if let Some(thread) = self.run_thread.take() {
            let _ = thread.join();
        }
        (result, bus)
    }
}

// ---------------------------------------------------------------------------
// §4.1 pilot e2e — TUI full flow
// ---------------------------------------------------------------------------

/// FR-A/R-Q5.1–Q5.2: faux tool call → real overlay mount → `Enter` submits
/// the focused option → envelope + `details.answers`; R-Q3.3 session JSONL
/// carries the answers; R-Q4.1/R-Q4.2 `prompt` then the `blocked` bracket
/// with the true arm before the dialog and the false arm last.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_single_question_submit_full_flow() {
    let Some(mut fixture) = TuiFixture::boot("single", &pick_one_params()).await else {
        return;
    };

    // Dialog frame on screen: option rows + footer hint.
    await_screen(&fixture.term, "Alpha", Duration::from_secs(10)).await;
    await_screen(&fixture.term, "Beta", Duration::from_secs(10)).await;

    // prompt event landed before the blocked pair (R-Q4.1 ordering).
    {
        let timeline = fixture
            .bus
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert!(
            timeline.iter().any(|(name, data)| *name == "prompt"
                && data["questions"][0]["question"] == "Pick one?"
                && data["questions"][0]["options"][0]["label"] == "Alpha"),
            "prompt payload: {timeline:?}"
        );
        assert!(
            timeline.first().map(|(name, _)| *name) == Some("prompt"),
            "prompt is emitted before the blocked bracket: {timeline:?}"
        );
    }

    // Enter submits the focused first option (R-Q5.2/Q5.11).
    fixture.term.feed("\r");
    let (result, bus) = fixture.finish().await;
    assert_alpha_envelope(&result);

    // Bus bracket: blocked:true → … → blocked:false, false arm last.
    let names: Vec<&str> = bus.iter().map(|(name, _)| name.as_str()).collect();
    let blocked_true = names
        .iter()
        .position(|name| *name == "blocked")
        .expect("blocked event");
    assert!(
        bus[blocked_true].1 == json!({ "active": true }),
        "first blocked arm is active:true: {bus:?}"
    );
    let last = bus.last().expect("closing blocked arm");
    assert_eq!(last.0, "blocked");
    assert_eq!(last.1, json!({ "active": false }));

    // R-Q3.3: the file-backed session JSONL carries the answers detail.
    let sessions_dir = fixture._sandbox.sessions();
    let deadline = Instant::now() + Duration::from_secs(10);
    let session_jsonl = loop {
        let entries = std::fs::read_dir(&sessions_dir)
            .map(|dir| {
                dir.filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let file = find_jsonl(&sessions_dir);
        if let Some(file) = file {
            break file;
        }
        assert!(
            Instant::now() < deadline,
            "session file never flushed; dir {:?} has {entries:?}",
            fixture._sandbox.sessions()
        );
        sleep(Duration::from_millis(50)).await;
    };
    let content = std::fs::read_to_string(&session_jsonl).expect("read session jsonl");
    let persisted = content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| {
            entry["type"] == "message"
                && entry["message"]["role"] == "toolResult"
                && entry["message"]["toolName"] == "ask_user_question"
        })
        .expect("toolResult entry in session file");
    assert_eq!(
        persisted["message"]["details"]["answers"][0]["kind"],
        "option"
    );
    assert_eq!(
        persisted["message"]["details"]["answers"][0]["answer"],
        "Alpha"
    );

    // The turn completed with the final assistant text.
    let texts: Vec<String> = fixture
        .session
        .messages()
        .into_iter()
        .filter_map(|message| serde_json::to_value(message).ok())
        .filter(|value| value["role"] == "assistant")
        .filter_map(|value| value["content"][0]["text"].as_str().map(str::to_owned))
        .collect();
    assert!(texts.iter().any(|text| text == "all done"), "{texts:?}");
}

fn find_jsonl(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
}

/// FR-A/R-Q5.1/Q5.4/Q5.6: two questions — a multi-select (Space + Next row)
/// then a single-select — Tab-free auto-advance onto the Submit tab, the
/// global note (`n` on Submit) and Submit; the envelope carries the multi
/// answer, the option answer and the global note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_multi_question_multi_select_notes_submit() {
    let params = json!({
        "questions": [
            {
                "question": "Pick many?",
                "header": "H1",
                "multiSelect": true,
                "options": [
                    {"label": "One", "description": "1"},
                    {"label": "Two", "description": "2"}
                ]
            },
            {
                "question": "Pick one?",
                "header": "H2",
                "options": [
                    {"label": "Alpha", "description": "alpha option"},
                    {"label": "Beta", "description": "beta option"}
                ]
            }
        ]
    });
    let Some(mut fixture) = TuiFixture::boot("multi", &params).await else {
        return;
    };
    // Multi-question dialogs show the tab bar (R-Q5.1).
    await_screen(&fixture.term, "H1", Duration::from_secs(10)).await;

    // q1 (multi-select): ↓ onto "Two", Space toggles it (the `[x]` glyph
    // lands in the frame), ↓×2 onto the Next sentinel row, Enter confirms
    // the selection and auto-advances (R-Q5.4).
    fixture.term.feed("\x1b[B"); // ↓ Two
    fixture.term.feed(" "); // Space toggles it
    await_screen(&fixture.term, "[x] Two", Duration::from_secs(10)).await;
    fixture.term.feed("\x1b[B"); // ↓ Type something.
    fixture.term.feed("\x1b[B"); // ↓ Next
    fixture.term.feed("\r");
    // q2: auto-advanced; Enter picks Alpha and auto-advances to Submit.
    await_screen(&fixture.term, "H2", Duration::from_secs(10)).await;
    fixture.term.feed("\r");
    // Submit tab (R-Q5.1): answers review + Submit/Cancel rows.
    await_screen(&fixture.term, "Submit", Duration::from_secs(10)).await;
    // Global note via `n` on the Submit tab (R-Q5.6), then commit + submit.
    fixture.term.feed("n");
    for character in "global note".chars() {
        fixture.term.feed(&character.to_string());
    }
    fixture.term.feed("\r"); // commit the note (Enter exits notes mode)
    fixture.term.feed("\r"); // Submit row

    let (result, _bus) = fixture.finish().await;
    assert_eq!(result["details"]["cancelled"], json!(false));
    assert_eq!(
        result["content"][0]["text"],
        concat!(
            "User has answered your questions: \"Pick many?\"=\"Two\". ",
            "\"Pick one?\"=\"Alpha\". ",
            "global note: global note. ",
            "You can now continue with the user's answers in mind."
        )
    );
    assert_eq!(result["details"]["answers"][0]["kind"], json!("multi"));
    assert_eq!(result["details"]["answers"][0]["selected"], json!(["Two"]));
    assert_eq!(result["details"]["answers"][1]["kind"], json!("option"));
    assert_eq!(result["details"]["globalNote"], json!("global note"));
}

/// FR-A/R-Q5.5/Q5.11: the `Type something.` row focuses into inline input;
/// printable keys build the draft, Ctrl+U clears it, `Enter` submits the
/// custom answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_type_something_custom_answer_with_ctrl_u() {
    let Some(mut fixture) = TuiFixture::boot("custom", &pick_one_params()).await else {
        return;
    };
    // Rows: 1. Alpha, 2. Beta, Type something. — ↓×2 focuses the sentinel
    // and enters inline input mode (R-Q5.5).
    fixture.term.feed("\x1b[B");
    fixture.term.feed("\x1b[B");
    for character in "hello world".chars() {
        fixture.term.feed(&character.to_string());
    }
    await_screen(&fixture.term, "hello world", Duration::from_secs(10)).await;
    // Ctrl+U clears the draft (R-Q5.5/Q5.11).
    fixture.term.feed("\x15");
    for character in "custom".chars() {
        fixture.term.feed(&character.to_string());
    }
    await_screen(&fixture.term, "custom", Duration::from_secs(10)).await;
    fixture.term.feed("\r");

    let (result, _bus) = fixture.finish().await;
    assert_eq!(
        result["content"][0]["text"],
        concat!(
            "User has answered your questions: \"Pick one?\"=\"custom\". ",
            "You can now continue with the user's answers in mind."
        )
    );
    assert_eq!(result["details"]["answers"][0]["kind"], json!("custom"));
    assert_eq!(result["details"]["answers"][0]["answer"], json!("custom"));
}

/// FR-A/R-Q5.7 + TE-D38: collapse hides the overlay with a one-time notify,
/// a normal key (Esc) while hidden is NOT delivered (hidden Esc cannot
/// cancel), the collapse key reopens the dialog, and the submit lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_collapse_reopen_hidden_esc_not_cancel() {
    let Some(mut fixture) = TuiFixture::boot("collapse", &pick_one_params()).await else {
        return;
    };
    let mounts_before = fixture.term.occurrences("Enter to select");

    // Ctrl+] collapses (R-Q5.7): overlay hides + one-time guidance notify.
    fixture.term.feed("\x1d");
    await_screen(
        &fixture.term,
        "ask_user_question hidden — press",
        Duration::from_secs(10),
    )
    .await;

    // Esc while hidden must not reach the component (keysWhenHidden only
    // routes the collapse key; TE-D38). Give it a real chance to misfire.
    fixture.term.feed("\x1b");
    sleep(Duration::from_millis(500)).await;
    assert!(
        askq_tool_result(&fixture.session).is_none(),
        "hidden Esc must not cancel the questionnaire (TE-D38)"
    );

    // Ctrl+] again reopens: the dialog frame is written back.
    fixture.term.feed("\x1d");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if fixture.term.occurrences("Enter to select") > mounts_before {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    assert!(
        fixture.term.occurrences("Enter to select") > mounts_before,
        "dialog did not reopen:\n{}",
        fixture.term.screen()
    );

    fixture.term.feed("\r");
    let (result, _bus) = fixture.finish().await;
    assert_alpha_envelope(&result);
}

/// FR-A/R-Q5.10: visible-state Esc declines the whole questionnaire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_esc_cancels_questionnaire() {
    let Some(mut fixture) = TuiFixture::boot("cancel", &pick_one_params()).await else {
        return;
    };
    fixture.term.feed("\x1b");
    let (result, bus) = fixture.finish().await;
    assert_eq!(result["content"][0]["text"], DECLINE_ENVELOPE);
    assert_eq!(result["details"]["cancelled"], json!(true));
    assert_eq!(result["details"]["answers"], json!([]));
    // The blocked bracket still closes (finally path).
    let last = bus.last().expect("closing blocked arm");
    assert_eq!(last.0, "blocked");
    assert_eq!(last.1, json!({ "active": false }));
}

/// FR-A/R-Q5.9: Ctrl+G in input mode opens the host external editor
/// (`ui.editExternal`, C3) with a 0600 temp file; the edited text replaces
/// the draft and submits as the custom answer.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_external_editor_ctrl_g_edits_draft() {
    let Some(mut fixture) = TuiFixture::boot("editor", &pick_one_params()).await else {
        return;
    };
    // Fake $VISUAL editor: overwrite the temp file with fixed text.
    let script = fixture._sandbox.root.join("fake-editor.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf 'edited by pilot' > \"$1\"\n").expect("editor");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod editor");
    unsafe { std::env::set_var("VISUAL", script.display().to_string()) };

    // Enter input mode with a small draft, then Ctrl+G (R-Q5.9/Q5.11).
    fixture.term.feed("\x1b[B");
    fixture.term.feed("\x1b[B");
    fixture.term.feed("d");
    await_screen(&fixture.term, "d", Duration::from_secs(10)).await;
    fixture.term.feed("\x07");
    await_screen(&fixture.term, "edited by pilot", Duration::from_secs(20)).await;
    fixture.term.feed("\r");

    let (result, _bus) = fixture.finish().await;
    assert_eq!(result["details"]["answers"][0]["kind"], json!("custom"));
    assert_eq!(
        result["details"]["answers"][0]["answer"],
        json!("edited by pilot")
    );
}

// ---------------------------------------------------------------------------
// §4.2 RPC / fallback e2e
// ---------------------------------------------------------------------------

/// FR-B/R-Q6.1: `ctx.mode == "rpc"` routes to the dialog walker; the real
/// RPC mode forwards `ui.select` as an `extension_ui_request` frame, the
/// client answer completes the round trip, and the envelope matches the TUI
/// path byte-for-byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_walker_full_round_trip() {
    let Some(plugin) = plugin_path() else {
        return;
    };
    let sandbox = Sandbox::new("rpc");
    let _env = EnvGuard::acquire(&sandbox.home());
    let plugin_dir = package_plugin(&sandbox, &plugin);
    let host = load_host(&plugin_dir, &sandbox.cwd()).await;
    let bus = record_bus(&host);

    let cwd = sandbox.cwd();
    let agent_dir = sandbox.agent_dir();
    let (model_runtime, model) = faux_model_runtime(
        &agent_dir,
        vec![tool_call_step(&pick_one_params()), text_step("rpc done")],
    )
    .await;
    let host_for_factory = host.clone();
    let factory_model = model.clone();
    let factory: rpi::core::agent_session_runtime::CreateAgentSessionRuntimeFactory =
        Arc::new(move |options| {
            let model_runtime = Arc::clone(&model_runtime);
            let model = factory_model.clone();
            let host = host_for_factory.clone();
            Box::pin(async move {
                let services = rpi::core::agent_session_services::create_agent_session_services(
                    rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
                        cwd: options.cwd.clone(),
                        agent_dir: Some(options.agent_dir.clone()),
                        settings_manager: None,
                        model_runtime: Some(model_runtime.clone()),
                        extension_flag_values: Vec::new(),
                        resource_loader_options: None,
                    },
                )
                .await?;
                let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
                    cwd: Some(options.cwd.clone()),
                    agent_dir: Some(options.agent_dir.clone()),
                    model: Some(model),
                    services: Some(services.clone()),
                    session_manager: Some(options.session_manager),
                    extension_host: Some(host.clone()),
                    ..Default::default()
                })
                .await?;
                rpi::core::extension_actions::bind_session_actions(&host, &created.session).await;
                Ok(
                    rpi::core::agent_session_runtime::CreateAgentSessionRuntimeResult {
                        session: created.session,
                        services: created.services.expect("services"),
                        diagnostics: Vec::new(),
                        model_fallback_message: created.model_fallback_message,
                    },
                )
            })
        });
    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let runtime = rpi::core::agent_session_runtime::create_agent_session_runtime(
        factory,
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

    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (_read_half, mut stdin) = tokio::io::split(client_io);
    let server_io = tokio::io::BufReader::new(server_io);
    let output = SharedBuf::default();
    let writer = output.clone();
    let handle = tokio::spawn(async move {
        rpi::modes::rpc::run_rpc_mode(runtime, server_io, Box::new(writer)).await
    });

    // Kick off the prompt; the walker then blocks on the client dialog.
    let prompt_line = serde_json::to_string(&json!({"type": "prompt", "id": "1", "message": "go"}))
        .expect("prompt line");
    stdin
        .write_all(format!("{prompt_line}\n").as_bytes())
        .await
        .expect("send prompt");

    // Wait for the extension_ui_request frame and answer it with the first
    // option line (the walker parses the leading index).
    let request = await_frame(&output, "extension_ui_request", Duration::from_secs(30)).await;
    assert_eq!(request["method"], json!("select"), "{request:?}");
    assert!(
        request["title"]
            .as_str()
            .is_some_and(|title| title.starts_with("[Pick] Pick one?")),
        "{}",
        request["title"]
    );
    let options = request["options"].as_array().expect("options");
    assert!(
        options
            .iter()
            .any(|option| option.as_str().is_some_and(|o| o.contains("Alpha"))),
        "{options:?}"
    );
    let answer = options
        .iter()
        .find_map(|option| option.as_str().filter(|o| o.contains("Alpha")))
        .expect("Alpha option line");
    assert!(answer.contains("1. Alpha — alpha option"), "{answer:?}");
    let response = serde_json::to_string(&json!({
        "type": "extension_ui_response",
        "id": request["id"],
        "value": answer,
    }))
    .expect("response line");
    stdin
        .write_all(format!("{response}\n").as_bytes())
        .await
        .expect("send response");

    // The turn completes: envelope identical to the TUI path (R-Q6.1).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(result) = output.askq_tool_result() {
            assert_alpha_envelope(&result);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rpc tool result never arrived; output:\n{}",
            String::from_utf8_lossy(&output.bytes())
        );
        sleep(Duration::from_millis(50)).await;
    }
    // blocked bracket closed.
    let blocked: Vec<Value> = bus
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .filter(|(name, _)| *name == "blocked")
        .map(|(_, data)| data.clone())
        .collect();
    assert_eq!(
        blocked.last().cloned(),
        Some(json!({ "active": false })),
        "closing blocked arm"
    );

    stdin.shutdown().await.expect("close stdin");
    let _ = tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("rpc mode exits");
}

#[derive(Clone, Default)]
struct SharedBuf {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl SharedBuf {
    fn bytes(&self) -> Vec<u8> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// The ask_user_question tool result from a `message_end` wire event
    /// (`message_end.message` is the authoritative final state).
    fn askq_tool_result(&self) -> Option<Value> {
        let text = String::from_utf8_lossy(&self.bytes()).into_owned();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["type"] == "message_end")
            .filter_map(|value| value.get("message").cloned())
            .find(|message| {
                message["role"] == "toolResult" && message["toolName"] == "ask_user_question"
            })
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Wait for a JSON frame with the given `type` field in the shared output.
async fn await_frame(output: &SharedBuf, frame_type: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let text = String::from_utf8_lossy(&output.bytes()).into_owned();
        for line in text.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                if value["type"] == frame_type {
                    return value;
                }
            }
        }
        sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "no {frame_type} frame; output:\n{}",
        String::from_utf8_lossy(&output.bytes())
    );
}

/// FR-B/R-Q6.2: a bridge without the interactive-UI ABI (pre-C1 host) makes
/// `ui.mountComponent` answer `unknownMethod`; the walker fallback answers
/// through `ui.select` and the envelope is identical to the TUI path. The
/// `no_custom_ui` arm (probe answers unavailable) is structurally
/// unreachable on the real host (`HostCallUi::probe` ≡ `ctx.hasUI`) and is
/// covered by the plugin's host-script unit tests + l0_load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_host_mount_unknown_method_falls_back_to_walker() {
    let Some(plugin) = plugin_path() else {
        return;
    };
    let sandbox = Sandbox::new("legacy");
    let _env = EnvGuard::acquire(&sandbox.home());
    let plugin_dir = package_plugin(&sandbox, &plugin);
    let host = load_host(&plugin_dir, &sandbox.cwd()).await;

    // TUI-mode bridge WITHOUT interactive-UI support (trait defaults answer
    // unknownMethod) but with scripted dialogs.
    let bridge = Arc::new(ScriptedDialogBridge {
        answer: Mutex::new(Some("1. Alpha — alpha option".to_owned())),
        selects: Mutex::new(Vec::new()),
    });
    host.set_ui(
        Some(bridge.clone() as Arc<dyn UiBridge>),
        rpi_ext_host::types::ExtensionMode::Tui,
    );

    let (model_runtime, model) = faux_model_runtime(
        &sandbox.agent_dir(),
        vec![tool_call_step(&pick_one_params()), text_step("legacy done")],
    )
    .await;
    let session = build_session(&sandbox, host.clone(), model_runtime, model, false).await;
    // The real app binds the session-backed host actions at boot
    // (app.rs); `create_agent_session` leaves them unbound. The reconciler
    // needs `getActiveTools`/`setActiveTools`, the walker needs nothing.
    rpi::core::extension_actions::bind_session_actions(&host, &session).await;

    session
        .prompt("go", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    session.wait_for_idle().await;

    let result = askq_tool_result(&session).expect("tool result through walker");
    assert_alpha_envelope(&result);
    let titles = bridge
        .selects
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(titles.len(), 1, "one walker dialog");
    assert!(titles[0].starts_with("[Pick] Pick one?"), "{}", titles[0]);
}

/// A minimal bridge with dialogs but no interactive-UI methods (the
/// "pre-C1 host" shape): `ui.mountComponent` answers the trait-default
/// `unknownMethod`.
struct ScriptedDialogBridge {
    answer: Mutex<Option<String>>,
    selects: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl UiBridge for ScriptedDialogBridge {
    async fn select(
        &self,
        title: &str,
        _options: &[String],
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        self.selects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(title.to_owned());
        self.answer.lock().unwrap_or_else(|e| e.into_inner()).take()
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
        None
    }
    fn notify(&self, _message: &str, _kind: NotifyType) {}
    fn on_terminal_input(&self, _handler: TerminalInputHandler) -> Unsubscribe {
        Box::new(|| {})
    }
    fn set_status(&self, _key: &str, _text: Option<&str>) {}
    fn set_working_message(&self, _message: Option<&str>) {}
    fn set_working_visible(&self, _visible: bool) {}
    fn set_working_indicator(&self, _options: Option<WorkingIndicatorOptions>) {}
    fn set_hidden_thinking_label(&self, _label: Option<&str>) {}
    fn set_widget(
        &self,
        _key: &str,
        _content: Option<WidgetContent>,
        _options: Option<ExtensionWidgetOptions>,
    ) {
    }
    fn set_footer(&self, _content: Option<Value>) {}
    fn set_header(&self, _content: Option<Value>) {}
    fn set_title(&self, _title: &str) {}
    async fn custom(&self, _content: Value, _options: Option<Value>) -> Option<Value> {
        None
    }
    fn paste_to_editor(&self, _text: &str) {}
    fn set_editor_text(&self, _text: &str) {}
    fn get_editor_text(&self) -> String {
        String::new()
    }
    async fn editor(&self, _title: &str, _prefill: Option<&str>) -> Option<String> {
        None
    }
    fn add_autocomplete_provider(&self, _provider: Value) {}
    fn set_editor_component(&self, _component: Option<Value>) {}
    fn get_editor_component(&self) -> Option<Value> {
        None
    }
    fn theme(&self) -> Value {
        Value::Null
    }
    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        Vec::new()
    }
    fn get_theme(&self, _name: &str) -> Option<Value> {
        None
    }
    fn set_theme(&self, _theme: Value) -> SetThemeResult {
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

/// FR-B/R-Q6.3: without a UI bridge the reconciler strips
/// `ask_user_question` from the model's tool list (`before_agent_start`),
/// and re-binding a bridge restores it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_ui_coordinator_strips_and_restores_tool() {
    let Some(plugin) = plugin_path() else {
        return;
    };
    let sandbox = Sandbox::new("noui");
    let _env = EnvGuard::acquire(&sandbox.home());
    let plugin_dir = package_plugin(&sandbox, &plugin);
    let host = load_host(&plugin_dir, &sandbox.cwd()).await;

    // Capture the tool list of every model request via factory steps.
    let seen_tools: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_a = seen_tools.clone();
    let seen_b = seen_tools.clone();
    let capture_a: FauxResponseStep =
        FauxResponseStep::Factory(Box::new(move |context, _options, _state, _model| {
            seen_a.lock().unwrap_or_else(|e| e.into_inner()).push(
                context
                    .tools
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect(),
            );
            faux_assistant_message("first", FauxAssistantOptions::default())
        }));
    let capture_b: FauxResponseStep =
        FauxResponseStep::Factory(Box::new(move |context, _options, _state, _model| {
            seen_b.lock().unwrap_or_else(|e| e.into_inner()).push(
                context
                    .tools
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect(),
            );
            faux_assistant_message("second", FauxAssistantOptions::default())
        }));

    let (model_runtime, model) =
        faux_model_runtime(&sandbox.agent_dir(), vec![capture_a, capture_b]).await;
    let session = build_session(&sandbox, host.clone(), model_runtime, model, false).await;
    rpi::core::extension_actions::bind_session_actions(&host, &session).await;

    // No bridge bound → hasUI=false → the reconciler strips the tool.
    session
        .prompt("first", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt 1");
    session.wait_for_idle().await;

    // Bridge bound (dialogs only — hasUI is about bridge presence) → the
    // reconciler re-adds the tool on the next prompt.
    host.set_ui(
        Some(Arc::new(ScriptedDialogBridge {
            answer: Mutex::new(None),
            selects: Mutex::new(Vec::new()),
        }) as Arc<dyn UiBridge>),
        rpi_ext_host::types::ExtensionMode::Tui,
    );
    session
        .prompt("second", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt 2");
    session.wait_for_idle().await;

    let seen = seen_tools.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(seen.len(), 2, "two model requests captured: {seen:?}");
    assert!(
        !seen[0].iter().any(|name| name == "ask_user_question"),
        "no-UI request hides the tool (R-Q6.3): {seen:?}"
    );
    assert!(
        seen[1].iter().any(|name| name == "ask_user_question"),
        "bridge restored the tool: {seen:?}"
    );
    assert!(
        seen[0].iter().any(|name| name == "read"),
        "sibling builtin tools untouched: {seen:?}"
    );
}
