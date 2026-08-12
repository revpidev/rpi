//! Shared test support for the interactive mode (S4b).
//!
//! [`TestTerminal`] is a minimal in-memory `Terminal` implementation for
//! building a `Tui` without a real tty (input feed + captured writes);
//! [`TestSession`] builds a real `AgentSession` over a temp agent dir with a
//! models.json, so the mode and footer tests exercise the actual session
//! plumbing (usage totals, context usage, entry lists).

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rpi_tui::terminal::{InputHandler, ResizeHandler, Terminal};
use tokio::time::Duration;

use crate::core::agent_session::AgentSession;
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use crate::core::session_manager::{NewSessionOptions, SessionManager};
use crate::sdk::{create_agent_session, CreateAgentSessionOptions, NoTools};

/// Serializes `std::env` mutations across interactive-mode tests: several
/// test modules override `RPI_CODING_AGENT_DIR` / `RPI_EXPERIMENTAL`, and
/// the process environment is shared by all tests running in parallel.
pub(crate) static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Temp dir with unique name (same pattern as model_runtime tests).
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "rpi-interactive-test-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// In-memory terminal for mode tests: captures writes, remembers the input
/// handler for `feed`, reports a fixed 80x24 size.
#[derive(Clone)]
pub(crate) struct TestTerminal {
    writes: Arc<Mutex<String>>,
    input_handler: Arc<Mutex<Option<InputHandler>>>,
    resize_handler: Arc<Mutex<Option<ResizeHandler>>>,
    title: Arc<Mutex<String>>,
    progress: Arc<Mutex<bool>>,
}

impl TestTerminal {
    pub(crate) fn new() -> Self {
        Self {
            writes: Arc::new(Mutex::new(String::new())),
            input_handler: Arc::new(Mutex::new(None)),
            resize_handler: Arc::new(Mutex::new(None)),
            title: Arc::new(Mutex::new(String::new())),
            progress: Arc::new(Mutex::new(false)),
        }
    }

    /// Feed raw input as if it came from the terminal (upstream
    /// `process.stdin.emit("data", ...)`). Used by future input-driven
    /// tests (S5 keybinding tests).
    #[allow(dead_code)]
    pub(crate) fn feed(&self, data: &str) {
        let mut handler = self.input_handler.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handler) = handler.as_mut() {
            handler(data);
        }
    }

    /// All bytes written to the terminal so far.
    #[allow(dead_code)]
    pub(crate) fn writes(&self) -> String {
        self.writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn title(&self) -> String {
        self.title.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[allow(dead_code)]
    pub(crate) fn progress(&self) -> bool {
        *self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `start` was called (input handler installed).
    pub(crate) fn is_started(&self) -> bool {
        self.input_handler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

impl Terminal for TestTerminal {
    fn start(&mut self, on_input: InputHandler, on_resize: ResizeHandler) {
        *self.input_handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(on_input);
        *self
            .resize_handler
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(on_resize);
    }

    fn stop(&mut self) {
        *self.input_handler.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
            .unwrap_or_else(|e| e.into_inner())
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

    fn set_title(&mut self, title: &str) {
        *self.title.lock().unwrap_or_else(|e| e.into_inner()) = title.to_string();
    }

    fn set_progress(&mut self, active: bool) {
        *self.progress.lock().unwrap_or_else(|e| e.into_inner()) = active;
    }

    fn pump(&mut self, _timeout: Option<Duration>) -> bool {
        // Keep the driver thread from hot-spinning in tests that run `run()`.
        std::thread::sleep(Duration::from_millis(2));
        false
    }
}

/// A built test session: real `AgentSession` (plus its [`AgentSessionRuntime`]
/// for mode construction) over a temp agent dir with a minimal models.json
/// (one `custom/m1` model, 200k context window) and an in-memory session
/// manager. The temp dir lives for the duration of the `TestSession` value.
pub(crate) struct TestSession {
    pub(crate) _tmp: TempDir,
    pub(crate) runtime: AgentSessionRuntime,
    pub(crate) session: AgentSession,
    pub(crate) cwd: PathBuf,
}

// ---------------------------------------------------------------------------
// No-op product-endpoint transports (T14 review M1)
// ---------------------------------------------------------------------------
//
// Unit tests must never emit real requests to product endpoints (revpi.dev
// install telemetry / version checks). Every mode test harness swaps the
// production transports for these no-ops via
// [`install_noop_product_transports`]; the optional counters let a test
// assert the injection actually carries the calls.

/// No-op [`ReportInstallTransport`]: records the invocation count, never
/// touches the network.
#[derive(Default)]
pub(crate) struct NoopReportInstallTransport(pub(crate) Arc<AtomicUsize>);

impl crate::core::telemetry::ReportInstallTransport for NoopReportInstallTransport {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        _user_agent: &'a str,
        _timeout: Duration,
    ) -> Pin<Box<dyn futures::Future<Output = Result<(), String>> + Send + 'a>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { Ok(()) })
    }
}

/// No-op [`LatestVersionTransport`]: records the invocation count, never
/// touches the network.
#[derive(Default)]
pub(crate) struct NoopLatestVersionTransport(pub(crate) Arc<AtomicUsize>);

impl crate::core::version_check::LatestVersionTransport for NoopLatestVersionTransport {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        _user_agent: &'a str,
        _timeout: Duration,
        _retry: bool,
    ) -> Pin<Box<dyn futures::Future<Output = Result<Option<String>, String>> + Send + 'a>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { Ok(None) })
    }
}

/// Swap both product-endpoint transports on a mode for the no-ops. Returns
/// the installed instances (their counters can be inspected by tests).
pub(crate) fn install_noop_product_transports(
    mode: &crate::modes::interactive::interactive_mode::InteractiveMode,
) -> (
    Arc<NoopReportInstallTransport>,
    Arc<NoopLatestVersionTransport>,
) {
    let report_install = Arc::new(NoopReportInstallTransport::default());
    let latest_version = Arc::new(NoopLatestVersionTransport::default());
    mode.ui_state.set_report_install_transport(
        Arc::clone(&report_install) as Arc<dyn crate::core::telemetry::ReportInstallTransport>
    );
    mode.ui_state
        .set_latest_version_transport(Arc::clone(&latest_version)
            as Arc<dyn crate::core::version_check::LatestVersionTransport>);
    (report_install, latest_version)
}

pub(crate) async fn build_test_session() -> TestSession {
    build_test_session_with_manager(None).await
}

/// [`build_test_session`] with an optional caller-provided session manager
/// (e.g. file-backed for `/export` / `/share` tests).
pub(crate) async fn build_test_session_with_manager(
    manager: Option<SessionManager>,
) -> TestSession {
    let tmp = TempDir::new();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("models.json"),
        r#"{"providers": {"custom": {
            "baseUrl": "https://api.example.com/v1",
            "api": "openai-completions",
            "apiKey": "RPI_TEST_INTERACTIVE_KEY",
            "models": [{"id": "m1", "contextWindow": 200000}]
        }}}"#,
    )
    .expect("write models.json");
    let cwd = tmp.path().join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd dir");

    let services = create_agent_session_services(CreateAgentSessionServicesOptions {
        cwd: cwd.clone(),
        agent_dir: Some(agent_dir.clone()),
        settings_manager: None,
        model_runtime: None,
        extension_flag_values: Vec::new(),
        resource_loader_options: None,
    })
    .await
    .expect("create services");

    let model = services
        .model_runtime
        .get_model("custom", "m1")
        .expect("test model must compose");

    let session_manager = match manager {
        Some(manager) => manager,
        None => SessionManager::in_memory(Some(&cwd), NewSessionOptions::default())
            .expect("in-memory session manager"),
    };
    let session_manager = Arc::new(Mutex::new(session_manager));

    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: Some(cwd.clone()),
        agent_dir: Some(agent_dir),
        model: Some(model),
        no_tools: Some(NoTools::All),
        services: Some(services.clone()),
        session_manager: Some(session_manager),
        ..Default::default()
    })
    .await
    .expect("create test session");

    // The mode tests exercise the skeleton only; the runtime's session
    // factory (session switching) is never invoked.
    let runtime = AgentSessionRuntime::new(
        created.session.clone(),
        services,
        Arc::new(|_options| {
            Box::pin(async {
                unreachable!("session creation is not exercised by interactive tests")
            })
        }),
        Vec::new(),
        None,
    );

    TestSession {
        _tmp: tmp,
        runtime,
        session: created.session,
        cwd,
    }
}
