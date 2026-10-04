//! V16-05 FR-B integration: the permission-mode API, the `mode_change`
//! event payloads, and the interactive/non-interactive gate over a real
//! host + session + extension pipeline.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_test_support::faux::{FauxAiProvider, FauxProvider, FauxProviderOptions};
use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rpi-perm-mode-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    api: ExtensionApi,
    session: rpi::core::agent_session::AgentSession,
    events: Arc<Mutex<Vec<Value>>>,
    _tmp: TempDir,
}

async fn fixture() -> Fixture {
    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let slot: Arc<Mutex<Option<ExtensionApi>>> = Arc::new(Mutex::new(None));
    let events_for_factory = events.clone();
    let slot_for_factory = slot.clone();
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        *slot_for_factory
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(api.clone());
        let events = events_for_factory.clone();
        api.on(
            "mode_change",
            Arc::new(move |payload: Value, _ctx| {
                events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(payload);
                Box::pin(async { Ok(Value::Null) })
                    as std::pin::Pin<
                        Box<dyn std::future::Future<Output = Result<Value, String>> + Send>,
                    >
            }),
        )
        .expect("subscribe mode_change");
        Box::pin(async { Ok(()) })
    });

    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let provider = FauxProvider::new(FauxProviderOptions::default());
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

    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));

    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let errors = host
        .load_inline(&[InlineExtension::Anonymous(factory)])
        .await;
    assert!(errors.is_empty(), "load errors: {errors:?}");

    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host.clone()),
        ..Default::default()
    })
    .await
    .expect("create session");

    rpi::core::extension_actions::bind_session_actions(&host, &created.session).await;

    let api = slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .expect("factory ran");
    Fixture {
        api,
        session: created.session,
        events,
        _tmp: tmp,
    }
}

async fn wait_for_events(events: &Arc<Mutex<Vec<Value>>>, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let seen = events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if seen.len() >= count {
            return seen;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} mode_change events; saw {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn mark_interactive(session: &rpi::core::agent_session::AgentSession) {
    use rpi::core::agent_session::ExtensionBindings;
    use rpi::core::extensions::ExtensionMode;
    session
        .bind_extensions(ExtensionBindings {
            mode: Some(ExtensionMode::Interactive),
            on_error: None,
            shutdown: None,
        })
        .await;
}

/// Non-interactive sessions answer `default` and ignore `setMode` (V16-05
/// §8-5 implementation decision).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_interactive_sessions_answer_default() {
    let fixture = fixture().await;
    assert_eq!(fixture.api.get_mode().unwrap(), "default");
    fixture.api.set_mode("plan").unwrap();
    assert_eq!(fixture.api.get_mode().unwrap(), "default");
    assert!(
        fixture
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty(),
        "a no-op setMode must not dispatch"
    );
}

/// Interactive round trip: `setMode` → `mode_change`, same value no event,
/// cycle goes through the same authority, invalid values are ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interactive_round_trip_and_event_payloads() {
    use rpi::core::permission_mode::PermissionMode;

    let fixture = fixture().await;
    mark_interactive(&fixture.session).await;

    fixture.api.set_mode("plan").unwrap();
    assert_eq!(fixture.api.get_mode().unwrap(), "plan");
    let events = wait_for_events(&fixture.events, 1).await;
    assert_eq!(events[0]["type"], "mode_change");
    assert_eq!(events[0]["from"], "default");
    assert_eq!(events[0]["to"], "plan");

    // Same value → no second event.
    fixture.api.set_mode("plan").unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        fixture
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len(),
        1
    );

    // The keybinding path (`cycle_permission_mode`) shares the same
    // authority and dispatches the reverse transition.
    assert_eq!(
        fixture.session.cycle_permission_mode(),
        PermissionMode::Default
    );
    let events = wait_for_events(&fixture.events, 2).await;
    assert_eq!(events[1]["from"], "plan");
    assert_eq!(events[1]["to"], "default");

    // Unknown wire values are ignored (no state change, no event).
    fixture.api.set_mode("bogus").unwrap();
    assert_eq!(fixture.api.get_mode().unwrap(), "default");
    assert_eq!(
        fixture
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len(),
        2
    );
}

/// Session reset: the fresh session starts `Default`; the notification the
/// interactive rebind dispatches carries the `plan → default` transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_reset_notification_dispatches_plan_to_default() {
    use rpi::core::permission_mode::PermissionMode;

    let fixture = fixture().await;
    mark_interactive(&fixture.session).await;
    fixture.api.set_mode("plan").unwrap();
    let _ = wait_for_events(&fixture.events, 1).await;

    fixture
        .session
        .notify_permission_mode_reset(PermissionMode::Plan);
    let events = wait_for_events(&fixture.events, 2).await;
    assert_eq!(events[1]["from"], "plan");
    assert_eq!(events[1]["to"], "default");

    // Default → no reset notification.
    fixture
        .session
        .notify_permission_mode_reset(PermissionMode::Default);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        fixture
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len(),
        2
    );
}
