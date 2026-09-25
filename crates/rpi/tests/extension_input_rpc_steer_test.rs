//! #8718 (faa9863cb): direct `steer` / `follow_up` calls run through the
//! extension `input` handlers, preserving the caller's input source.
//!
//! Upstream anchor: `test/suite/agent-session-queue.test.ts` — "runs
//! direct steering and follow-up messages through input handlers". The
//! rpi adaptation drives `AgentSession::steer`/`follow_up` with
//! `InputSource::Rpc` (the RPC wire path passes exactly that) while the
//! agent streams, asserting:
//! - every queued message sees the handler (recorded `text`/`source`/
//!   `streamingBehavior`),
//! - `handled` results drop the message (nothing queues),
//! - `transform` results queue the transformed text.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use serde_json::{json, Value};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rpi-input-steer-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct SeenInput {
    text: String,
    source: String,
    streaming_behavior: Option<String>,
}

async fn fixture() -> (
    rpi::core::agent_session::AgentSession,
    Arc<Mutex<Vec<SeenInput>>>,
    TempDir,
) {
    let tmp = TempDir::new("8718");
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let seen: Arc<Mutex<Vec<SeenInput>>> = Arc::new(Mutex::new(Vec::new()));

    let seen_ext = seen.clone();
    let register = move |api: &rpi_ext_host::api::ExtensionApi| {
        let seen = seen_ext.clone();
        api.on(
            "input",
            Arc::new(move |payload: Value, _ctx| {
                let event: SeenInput = serde_json::from_value(json!({
                    "text": payload.get("text").cloned().unwrap_or(Value::Null),
                    "source": payload.get("source").cloned().unwrap_or(Value::Null),
                    "streaming_behavior": payload.get("streamingBehavior").cloned().unwrap_or(Value::Null),
                }))
                .unwrap_or(SeenInput {
                    text: String::new(),
                    source: String::new(),
                    streaming_behavior: None,
                });
                seen.lock().unwrap().push(event);
                let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
                if text.starts_with("handle") {
                    Box::pin(async { Ok(json!({"action": "handled"})) })
                        as std::pin::Pin<
                            Box<dyn std::future::Future<Output = Result<Value, String>> + Send>,
                        >
                } else {
                    let transformed = format!("transformed: {text}");
                    Box::pin(async move { Ok(json!({"action": "transform", "text": transformed})) })
                }
            }),
        )
        .expect("register input handler");
    };
    let factory: ExtensionFactory = Arc::new(move |api| {
        register(&api);
        Box::pin(async { Ok(()) })
    });
    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let errors = host
        .load_inline(&[InlineExtension::Anonymous(factory)])
        .await;
    assert!(errors.is_empty(), "load errors: {errors:?}");

    let provider =
        rpi_test_support::faux::FauxProvider::new(rpi_test_support::faux::FauxProviderOptions {
            tokens_per_second: Some(60.0),
            ..Default::default()
        });
    // One long streaming turn for the background prompt + short answers for
    // the steered/follow-up turns (only the queueing matters here).
    provider.set_responses(vec![
        rpi_test_support::faux::faux_assistant_message(
            "word ".repeat(400).as_str(),
            rpi_test_support::faux::FauxAssistantOptions::default(),
        )
        .into(),
        rpi_test_support::faux::faux_assistant_message(
            "steered",
            rpi_test_support::faux::FauxAssistantOptions::default(),
        )
        .into(),
        rpi_test_support::faux::faux_assistant_message(
            "followed up",
            rpi_test_support::faux::FauxAssistantOptions::default(),
        )
        .into(),
    ]);
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
        .register_native_provider(Arc::new(rpi_test_support::faux::FauxAiProvider::new(
            provider,
        )))
        .await
        .expect("register faux provider");

    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir),
            settings_manager: None,
            model_runtime: Some(model_runtime),
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

    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: None,
        model_runtime: None,
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host),
        ..Default::default()
    })
    .await
    .expect("create session");

    (created.session, seen, tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_steer_and_follow_up_run_input_handlers() {
    use rpi::core::agent_session::PromptOptions;
    use rpi::core::extensions::{InputSource, StreamingBehavior};

    let (session, seen, _tmp) = fixture().await;

    // Background prompt streams slowly; steer/followUp land mid-stream.
    let prompt_session = session.clone();
    let prompt_task = tokio::spawn(async move {
        prompt_session
            .prompt("start", PromptOptions::default())
            .await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(session.is_streaming(), "background prompt is streaming");
    seen.lock().unwrap().clear();

    session
        .steer("steer me", None, InputSource::Rpc)
        .await
        .expect("steer");
    session
        .steer("handle steer", None, InputSource::Rpc)
        .await
        .expect("steer handled");
    session
        .follow_up("follow me", None, InputSource::Rpc)
        .await
        .expect("follow up");
    session
        .follow_up("handle follow", None, InputSource::Rpc)
        .await
        .expect("follow up handled");

    // Every message saw the handler with the RPC source preserved and the
    // streaming behavior of its queue (agent-session-queue.test.ts:174-179).
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![
            SeenInput {
                text: "steer me".to_owned(),
                source: "rpc".to_owned(),
                streaming_behavior: Some("steer".to_owned()),
            },
            SeenInput {
                text: "handle steer".to_owned(),
                source: "rpc".to_owned(),
                streaming_behavior: Some("steer".to_owned()),
            },
            SeenInput {
                text: "follow me".to_owned(),
                source: "rpc".to_owned(),
                streaming_behavior: Some("followUp".to_owned()),
            },
            SeenInput {
                text: "handle follow".to_owned(),
                source: "rpc".to_owned(),
                streaming_behavior: Some("followUp".to_owned()),
            },
        ]
    );

    // `handled` messages never queue; `transform` results queue the
    // transformed text (agent-session-queue.test.ts:180-183).
    assert_eq!(
        session.get_steering_messages(),
        vec!["transformed: steer me".to_owned()]
    );
    assert_eq!(
        session.get_follow_up_messages(),
        vec!["transformed: follow me".to_owned()]
    );

    prompt_task.await.expect("prompt task").expect("prompt");
    session.wait_for_idle().await;
    let _ = StreamingBehavior::Steer; // import anchor
}
