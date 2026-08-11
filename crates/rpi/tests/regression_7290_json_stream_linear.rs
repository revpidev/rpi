//! Port of `test/suite/regressions/7290-json-stream-linear.test.ts`
//! @ pi 0.84.1+ (4181f66) — regression #7290: JSON event streams stay linear.
//!
//! Asserts both halves of the T18 contract:
//! - internal session events keep the cumulative `message` and
//!   `assistantMessageEvent.partial` snapshots (upstream keeps them too; the
//!   7290 test pins this);
//! - after `to_json_event` the wire events are delta-only, so the total
//!   `message_update` byte volume grows linearly with the response size
//!   (2000 vs 4000 chars, ratio < 2.2) instead of quadratically.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use rpi::core::agent_session::AgentSessionEvent;
use rpi::core::agent_session_runtime::{
    create_agent_session_runtime, AgentSessionRuntime, CreateAgentSessionRuntimeFactory,
    CreateAgentSessionRuntimeResult, CreateRuntimeOptions,
};
use rpi::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi::modes::json_event::to_json_event;
use rpi_agent::types::AgentEvent;
use rpi_test_support::faux::{
    faux_assistant_message, FauxAiProvider, FauxAssistantOptions, FauxModelDefinition,
    FauxProvider, FauxProviderOptions, FauxResponseStep,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-7290-{name}-{}-{nanos}-{id}",
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

/// Full-stack runtime over the faux provider (same assembly as
/// `parity_headless_test.rs::build_runtime`).
async fn build_runtime(
    name: &str,
    responses: Vec<FauxResponseStep>,
) -> (AgentSessionRuntime, TempDir) {
    let tmp = TempDir::new(name);
    let cwd = tmp.path().join("workspace");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: Some(true),
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
    provider.set_responses(responses);
    let model = provider.get_model(None).expect("faux model");

    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: None,
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
        ..Default::default()
    })
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider)))
        .await
        .expect("register faux provider");

    let factory: CreateAgentSessionRuntimeFactory = {
        let model_runtime = model_runtime.clone();
        Arc::new(move |options: CreateRuntimeOptions| {
            let model_runtime = model_runtime.clone();
            let model = model.clone();
            Box::pin(async move {
                let services = create_agent_session_services(CreateAgentSessionServicesOptions {
                    cwd: options.cwd.clone(),
                    agent_dir: Some(options.agent_dir.clone()),
                    settings_manager: None,
                    model_runtime: Some(model_runtime.clone()),
                    extension_flag_values: Vec::new(),
                    resource_loader_options: None,
                })
                .await?;
                let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
                    cwd: Some(options.cwd.clone()),
                    agent_dir: Some(options.agent_dir.clone()),
                    model_runtime: Some(model_runtime.clone()),
                    model: Some(model),
                    services: Some(services),
                    session_manager: Some(options.session_manager),
                    session_start_event: options.session_start_event,
                    ..Default::default()
                })
                .await?;
                Ok(CreateAgentSessionRuntimeResult {
                    session: created.session,
                    services: created.services.expect("services passed through"),
                    diagnostics: Vec::new(),
                    model_fallback_message: created.model_fallback_message,
                })
            })
        })
    };

    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory(Some(&cwd), NewSessionOptions::default())
            .expect("in-memory session"),
    ));
    let runtime = create_agent_session_runtime(
        factory,
        CreateRuntimeOptions {
            cwd,
            agent_dir,
            session_manager,
            session_start_event: None,
            project_trust_context: None,
        },
    )
    .await
    .expect("create runtime");
    (runtime, tmp)
}

/// `measureUpdateBytes` (7290-json-stream-linear.test.ts:15-35): run one
/// prompt whose faux response is `text`, then total the serialized size of
/// the wire-form `message_update` events.
async fn measure_update_bytes(text: &str) -> usize {
    let (runtime, _tmp) = build_runtime(
        "measure",
        vec![faux_assistant_message(text, FauxAssistantOptions::default()).into()],
    )
    .await;

    // `harness.eventsOfType("message_update")`: capture the internal session
    // events (they keep the cumulative snapshots by design).
    let events: Arc<Mutex<Vec<AgentSessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let _unsubscribe = runtime
        .session()
        .subscribe(Arc::new(move |event: AgentSessionEvent| {
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        }));

    runtime
        .session()
        .prompt("respond", Default::default())
        .await
        .expect("prompt");

    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    let session_updates: Vec<&AgentSessionEvent> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentSessionEvent::Agent(agent_event)
                    if matches!(&**agent_event, AgentEvent::MessageUpdate { .. })
            )
        })
        .collect();
    assert!(
        !session_updates.is_empty(),
        "expected message_update events for a streamed response"
    );

    let mut bytes = 0;
    for update in &session_updates {
        // Internal events still carry the cumulative snapshots
        // (7290 test: `expect(update).toHaveProperty("message")` /
        // `expect(update.assistantMessageEvent).toHaveProperty("partial")`).
        let internal = serde_json::to_value(update).expect("serialize internal event");
        assert!(
            internal.get("message").is_some(),
            "internal message_update must keep `message`: {internal}"
        );
        assert!(
            internal["assistantMessageEvent"].get("partial").is_some(),
            "internal message_update must keep `assistantMessageEvent.partial`: {internal}"
        );

        // Wire events are delta-only (7290 test: `not.toHaveProperty` both).
        let wire = to_json_event(update);
        assert!(
            wire.get("message").is_none(),
            "wire message_update must drop `message`: {wire}"
        );
        assert!(
            wire["assistantMessageEvent"].get("partial").is_none(),
            "wire message_update must drop `assistantMessageEvent.partial`: {wire}"
        );
        bytes += serde_json::to_string(&wire)
            .expect("serialize wire event")
            .len();
    }
    bytes
}

/// `it("emits delta-only message updates whose size scales linearly")`
/// (7290-json-stream-linear.test.ts:37-43).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn emits_delta_only_message_updates_whose_size_scales_linearly() {
    let small_bytes = measure_update_bytes(&"x".repeat(2_000)).await;
    let large_bytes = measure_update_bytes(&"x".repeat(4_000)).await;

    assert!(
        large_bytes > small_bytes,
        "large ({large_bytes}) should exceed small ({small_bytes})"
    );
    let ratio = large_bytes as f64 / small_bytes as f64;
    assert!(
        ratio < 2.2,
        "wire bytes must scale linearly, ratio {ratio:.2} (small {small_bytes}, large {large_bytes})"
    );
}

/// T18 self-check (task list): `message_end.message` is the authoritative
/// final state — assembling every `text_delta` delta off the wire must
/// reproduce it exactly (docs/rpc.md:952-956).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_end_message_matches_assembled_deltas() {
    let text = "The quick brown fox jumps over the lazy dog. ".repeat(20);
    let (runtime, _tmp) = build_runtime(
        "assemble",
        vec![faux_assistant_message(text.as_str(), FauxAssistantOptions::default()).into()],
    )
    .await;

    let events: Arc<Mutex<Vec<AgentSessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let _unsubscribe = runtime
        .session()
        .subscribe(Arc::new(move |event: AgentSessionEvent| {
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        }));
    runtime
        .session()
        .prompt("write text", Default::default())
        .await
        .expect("prompt");

    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    let mut assembled = String::new();
    let mut final_text: Option<String> = None;
    for event in events.iter() {
        let wire = to_json_event(event);
        match wire.get("type").and_then(Value::as_str) {
            Some("message_update") => {
                let delta_event = &wire["assistantMessageEvent"];
                if delta_event["type"].as_str() == Some("text_delta") {
                    // Assemble via `contentIndex` + `delta` (single text
                    // block here, so plain concatenation is the assembly).
                    assembled.push_str(delta_event["delta"].as_str().expect("delta string"));
                }
            }
            Some("message_end") => {
                final_text = Some(
                    wire["message"]["content"][0]["text"]
                        .as_str()
                        .expect("final text")
                        .to_owned(),
                );
            }
            _ => {}
        }
    }
    let final_text = final_text.expect("message_end emitted");
    assert!(!assembled.is_empty(), "expected text_delta events");
    assert_eq!(assembled, final_text);
    assert_eq!(assembled, text);
}
