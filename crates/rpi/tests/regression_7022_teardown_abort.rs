//! Port of `test/suite/regressions/7022-teardown-abort.test.ts`
//! @ pi 0.84.1+ (4181f66) — regression #7022 (R3.4.9): teardown must settle the
//! active response before emitting `session_shutdown`, so the aborted turn
//! (including tool results) is persisted to the outgoing session file before
//! it is replaced.
//!
//! Upstream `teardownCurrent` (agent-session-runtime.ts:167-178):
//! ```text
//! await this.session.abort();           // settle active turn first
//! await emitSessionShutdownEvent(...);  // then notify extensions
//! this.beforeSessionInvalidate?.();
//! this.session.dispose();
//! ```
//!
//! Before the fix the Rust port emitted `session_shutdown` *before*
//! `session.dispose()` (which calls abort internally but without waiting for
//! idle), risking dropped tool-result persistence on the outgoing session.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use rpi_agent::types::AgentEvent;
use rpi_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
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
            "rpi-7022-{name}-{}-{nanos}-{id}",
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

/// Full-stack runtime over the faux provider with a **persisted** session
/// (file-backed, not in-memory), so we can inspect the outgoing session file
/// after teardown.
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

    let session_dir = cwd.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");

    let factory: CreateAgentSessionRuntimeFactory = {
        let model_runtime = model_runtime.clone();
        let model = model.clone();
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
        SessionManager::create(&cwd, Some(&session_dir), NewSessionOptions::default())
            .expect("persisted session"),
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

/// Read a `.jsonl` session file and collect all parsed JSON lines.
fn read_session_jsonl(path: &Path) -> Vec<Value> {
    let content = std::fs::read_to_string(path).expect("read session file");
    content
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("parse jsonl line"))
        .collect()
}

/// #7022: when `new_session` triggers `teardown_current`, the outgoing
/// session's assistant message (with tool call) and tool result must already
/// be persisted before the session is replaced.
///
/// The `teardown_current` fix ensures `session.abort().await` settles the
/// turn first — so all `MessageEnd` events (including tool calls) have been
/// written to the session file before `session_shutdown` is emitted and
/// `dispose()` tears down listeners.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_persists_in_flight_turn_before_replacement() {
    // Two-step response: first message has a tool call (stop_reason: tool_use),
    // second message is a plain text response. After the tool executes, the
    // tool result is persisted. The prompt completes the full turn.
    let tool_call = faux_tool_call(
        "tool_a",
        serde_json::json!({"path": "/x"})
            .as_object()
            .unwrap()
            .clone(),
        None,
    );
    let first_message = faux_assistant_message(
        vec![tool_call],
        FauxAssistantOptions {
            stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
            ..Default::default()
        },
    );
    let second_message = faux_assistant_message("done", FauxAssistantOptions::default());

    let (mut runtime, _tmp) = build_runtime(
        "persist-before-replace",
        vec![first_message.into(), second_message.into()],
    )
    .await;

    let session_file = runtime
        .session()
        .session_file()
        .expect("persisted session has a file");

    // Run a full prompt — the turn completes (tool call → result → second
    // assistant message), and all messages are persisted.
    runtime
        .session()
        .prompt("test prompt", Default::default())
        .await
        .expect("prompt");

    // Now trigger new_session which calls teardown_current.
    // teardown_current calls abort() first, which is a no-op for an idle
    // session, then emits session_shutdown, then dispose().
    runtime
        .new_session(None, None, None)
        .await
        .expect("new_session");

    // The outgoing session file must contain the full turn.
    let entries = read_session_jsonl(&session_file);

    // Session header present.
    assert!(
        entries
            .iter()
            .any(|e| e.get("type") == Some(&Value::String("session".to_owned()))),
        "session header in outgoing file"
    );

    // User message persisted.
    assert!(
        entries.iter().any(|e| {
            e.get("type") == Some(&Value::String("message".to_owned()))
                && e.get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("user")
        }),
        "user message persisted to outgoing session file (#7022)"
    );

    // Assistant message with tool call persisted.
    assert!(
        entries.iter().any(|e| {
            e.get("type") == Some(&Value::String("message".to_owned()))
                && e.get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("assistant")
        }),
        "assistant message persisted to outgoing session file (#7022)"
    );

    // Tool result persisted (role: toolResult).
    assert!(
        entries.iter().any(|e| {
            e.get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                == Some("toolResult")
        }),
        "tool result persisted to outgoing session file (#7022)"
    );
}

/// #7022 (ordering): when `teardown_current` is called on a session with an
/// active turn, `abort()` must settle the turn before `session_shutdown` is
/// emitted. We verify this by checking that after the first `MessageEnd`
/// (assistant with tool call), a `new_session` that aborts the in-flight turn
/// results in the tool call being persisted to the outgoing file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_aborts_active_turn_before_shutdown_event() {
    // Single response with a tool call — the agent will execute the tool and
    // then attempt a second response, but we trigger new_session after the
    // first MessageEnd (tool call assistant message).
    let tool_call = faux_tool_call(
        "tool_a",
        serde_json::json!({"path": "/y"})
            .as_object()
            .unwrap()
            .clone(),
        None,
    );
    let first_message = faux_assistant_message(
        vec![tool_call],
        FauxAssistantOptions {
            stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
            ..Default::default()
        },
    );

    let (mut runtime, _tmp) = build_runtime("abort-active", vec![first_message.into()]).await;

    let session_file = runtime
        .session()
        .session_file()
        .expect("persisted session has a file");

    // Track when the first assistant message_end fires.
    let first_end = Arc::new(AtomicBool::new(false));
    let flag = first_end.clone();
    let _unsub = runtime.session().subscribe(Arc::new(move |event| {
        if let AgentSessionEvent::Agent(agent_event) = &event {
            if matches!(&**agent_event, AgentEvent::MessageEnd { .. }) {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }));

    // Start the prompt in a background task so we can trigger new_session
    // while the turn is in flight.
    let session = runtime.session().clone();
    let prompt_handle = tokio::spawn(async move {
        let _ = session.prompt("go", Default::default()).await;
    });

    // Wait for the first assistant message_end (tool call has been produced).
    for _ in 0..500 {
        if first_end.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        first_end.load(Ordering::SeqCst),
        "first assistant message_end should fire"
    );

    // Trigger new_session — teardown_current calls abort() first, which
    // settles the in-flight turn (persisting the assistant message with
    // tool call), then emits session_shutdown.
    let _ = runtime.new_session(None, None, None).await;
    // The prompt task should complete (abort settles the turn).
    let _ = prompt_handle.await;

    // Verify the outgoing session file has the assistant message persisted.
    let entries = read_session_jsonl(&session_file);
    assert!(
        entries.iter().any(|e| {
            e.get("type") == Some(&Value::String("message".to_owned()))
                && e.get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("assistant")
        }),
        "assistant message persisted to outgoing session file after teardown abort (#7022)"
    );
}
