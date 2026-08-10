//! SDK example test (self-check item: "call `create_agent_session` from
//! outside the crate and complete one faux conversation") — the Rust
//! counterpart of the `docs/sdk.md` "Quick Start":
//!
//! ```text
//! ModelRuntime::create → createAgentSession({ sessionManager: inMemory,
//! modelRuntime, model }) → session.subscribe(...) → session.prompt(...)
//! ```
//!
//! Assembled only through the `rpi::sdk` / public re-export surface, to
//! verify the SDK is usable from outside the crate.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi::core::agent_session::{AgentSessionEvent, SessionEvent};
use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime, ModelsPathInput};
use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi::sdk::{create_agent_session, CreateAgentSessionOptions};
use rpi_agent::types::AgentEvent;
use rpi_test_support::faux::{
    faux_assistant_message, FauxAiProvider, FauxAssistantOptions, FauxProvider, FauxProviderOptions,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-sdk-example-{}-{nanos}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_quick_start_one_faux_round() {
    let tmp = TempDir::new();
    let cwd = tmp.0.join("cwd");
    let agent_dir = tmp.0.join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    // Provider injection: the external caller's equivalent is
    // registerNativeProvider / registerProvider (here FauxProvider scripts one
    // conversation round).
    let provider = FauxProvider::new(FauxProviderOptions::default());
    provider.set_responses(vec![faux_assistant_message(
        "There are 3 files in the current directory.",
        FauxAssistantOptions::default(),
    )
    .into()]);

    let model_runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: None,
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
        ..Default::default()
    })
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register provider");

    let model = model_runtime
        .get_available(None)
        .await
        .expect("available models")
        .into_iter()
        .next()
        .expect("faux model available");

    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory(Some(&cwd), NewSessionOptions::default())
            .expect("in-memory session"),
    ));

    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        session_manager: Some(session_manager),
        ..Default::default()
    })
    .await
    .expect("create_agent_session");
    let session = created.session;
    assert!(created.model_fallback_message.is_none());

    // subscribe: event stream (the text_delta print loop from Quick Start).
    let events: Arc<Mutex<Vec<AgentSessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collector = events.clone();
    let _unsubscribe = session.subscribe(Arc::new(move |event| {
        collector
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }));

    session
        .prompt(
            "What files are in the current directory?",
            Default::default(),
        )
        .await
        .expect("prompt");

    assert_eq!(
        session.get_last_assistant_text().as_deref(),
        Some("There are 3 files in the current directory.")
    );
    assert_eq!(provider.call_count(), 1);

    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentSessionEvent::Agent(boxed) if matches!(**boxed, AgentEvent::AgentStart))));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentSessionEvent::AgentEnd(_))));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentSessionEvent::Session(SessionEvent::AgentSettled))));
}
