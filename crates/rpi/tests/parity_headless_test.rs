//! T10 three-mode parity: `fixtures/generated/<scenario>/` (recorded from
//! the upstream `createAgentSession` + faux) vs rpi's `AgentSession`
//! (full-stack assembly) in print/json modes.
//!
//! The scenario scripts are ported entry by entry from
//! `fixtures/generate-fixtures.mjs` (same prompts, steer/followUp/abort
//! timing, tool-call arguments).
//!
//! Normalization conventions (following the parity_compaction_test /
//! parity_events_test precedent):
//! - the whole `message_update` / `tool_execution_update` event classes are
//!   excluded: the upstream faux slices deltas with `Math.random`, so delta
//!   boundaries and output chunking are not part of the contract.
//! - the `usage` / `details` keys are stripped: usage is estimated from the
//!   full context (including the upstream builder's system prompt) and
//!   details is a tool-internal description shape — neither belongs to the
//!   session/event wire-format contract.
//! - the session header `cwd` is replaced with a placeholder.
//!
//! The compaction scenario is not repeated here:
//! `parity_compaction_test.rs` (T08) covers the threshold/overflow scenarios
//! under the same normalization.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use rpi::core::agent_session::{AgentSession, AgentSessionEvent, PromptOptions};
use rpi::core::agent_session_runtime::{
    create_agent_session_runtime, AgentSessionRuntime, CreateAgentSessionRuntimeFactory,
    CreateAgentSessionRuntimeResult, CreateRuntimeOptions,
};
use rpi::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi::modes::print_mode::{run_print_mode, PrintModeOptions, PrintOutputMode};
use rpi_agent::types::AgentEvent;
use rpi_ai::types::StopReason;
use rpi_test_support::diff::diff_jsonl;
use rpi_test_support::faux::{
    faux_assistant_message, faux_text, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated")
}

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
            "rpi-parity-headless-{name}-{}-{nanos}-{id}",
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

// ---------------------------------------------------------------------------
// Normalization preparation (per the header-note convention)
// ---------------------------------------------------------------------------

const STRIPPED_KEYS: &[&str] = &["usage", "details"];
const DROPPED_EVENT_TYPES: &[&str] = &["message_update", "tool_execution_update"];

fn strip_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !STRIPPED_KEYS.contains(&key.as_str()));
            for (_, val) in map.iter_mut() {
                strip_keys(val);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_keys(item);
            }
        }
        _ => {}
    }
}

fn prepare_session_lines(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_str(line).expect("session line is JSON");
        if value.get("type").and_then(Value::as_str) == Some("session") {
            value["cwd"] = Value::from("<cwd>");
        }
        strip_keys(&mut value);
        out.push_str(&serde_json::to_string(&value).expect("render"));
        out.push('\n');
    }
    out
}

fn prepare_event_lines(text: &str) -> String {
    // First filter by type and strip keys, then sort consecutive `tool_execution_end`
    // blocks by (toolCallId, toolName): upstream runs tools in parallel, so completion
    // order is timing-nondeterministic (this recording has bash before read) and is not
    // part of the contract.
    let mut prepared: Vec<Value> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_str(line).expect("event line is JSON");
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if DROPPED_EVENT_TYPES.contains(&event_type) {
            continue;
        }
        strip_keys(&mut value);
        prepared.push(value);
    }
    let mut out = String::new();
    let mut index = 0;
    while index < prepared.len() {
        if prepared[index]["type"] == "tool_execution_end" {
            let mut block: Vec<&Value> = Vec::new();
            while index < prepared.len() && prepared[index]["type"] == "tool_execution_end" {
                block.push(&prepared[index]);
                index += 1;
            }
            block.sort_by_key(|value| {
                (
                    value["toolCallId"].as_str().unwrap_or("").to_owned(),
                    value["toolName"].as_str().unwrap_or("").to_owned(),
                )
            });
            for value in block {
                out.push_str(&serde_json::to_string(value).expect("render"));
                out.push('\n');
            }
        } else {
            out.push_str(&serde_json::to_string(&prepared[index]).expect("render"));
            out.push('\n');
            index += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct ScenarioRun {
    session: AgentSession,
    events: Arc<Mutex<Vec<AgentSessionEvent>>>,
    session_file: PathBuf,
    _tmp: TempDir,
}

async fn start_scenario(
    name: &str,
    responses: Vec<FauxResponseStep>,
    provider_options: FauxProviderOptions,
    plant: impl FnOnce(&Path),
) -> ScenarioRun {
    let tmp = TempDir::new(name);
    let cwd = tmp.path().join("workspace");
    let agent_dir = tmp.path().join("agent");
    let session_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    plant(&cwd);

    let provider = FauxProvider::new(provider_options);
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
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux provider");

    let services = create_agent_session_services(CreateAgentSessionServicesOptions {
        cwd: cwd.clone(),
        agent_dir: Some(agent_dir.clone()),
        settings_manager: None,
        model_runtime: Some(model_runtime.clone()),
        extension_flag_values: Vec::new(),
        resource_loader_options: None,
    })
    .await
    .expect("services");

    let session_manager =
        SessionManager::create(&cwd, Some(&session_dir), NewSessionOptions::default())
            .expect("file-backed session");
    let session_file = session_manager
        .get_session_file()
        .expect("session file")
        .to_path_buf();
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(Arc::new(Mutex::new(session_manager))),
        ..Default::default()
    })
    .await
    .expect("create session");

    let events: Arc<Mutex<Vec<AgentSessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collector = events.clone();
    let _unsubscribe = created.session.subscribe(Arc::new(move |event| {
        collector
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }));
    std::mem::forget(_unsubscribe);

    ScenarioRun {
        session: created.session,
        events,
        session_file,
        _tmp: tmp,
    }
}

/// waits for the first `message_update` event from `since` onward (the timing anchor for
/// steer/followUp/abort, mirroring the generator's `waitForEvent("message_update")`;
/// `since` uses the event count at call time to avoid consuming events left over from the
/// previous prompt).
async fn wait_for_message_update(events: &Arc<Mutex<Vec<AgentSessionEvent>>>, since: usize) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        let seen = events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .skip(since)
            .any(|event| {
                matches!(
                    event,
                    AgentSessionEvent::Agent(boxed)
                        if matches!(**boxed, AgentEvent::MessageUpdate { .. })
                )
            });
        if seen {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timeout waiting for message_update"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn event_count(events: &Arc<Mutex<Vec<AgentSessionEvent>>>) -> usize {
    events.lock().unwrap_or_else(|e| e.into_inner()).len()
}

fn compare_with_fixture(scenario: &str, run: &ScenarioRun) {
    // Session file parity.
    let actual_session = std::fs::read_to_string(&run.session_file).expect("read actual session");
    let expected_session =
        std::fs::read_to_string(fixtures_dir().join(scenario).join("session.jsonl"))
            .expect("read fixture session");
    diff_jsonl(
        &prepare_session_lines(&expected_session),
        &prepare_session_lines(&actual_session),
    )
    .unwrap_or_else(|f| panic!("{scenario}: session parity diff:\n{f}"));

    // Event stream parity (same event shape as json mode).
    let mut actual_events = String::new();
    for event in run.events.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        actual_events.push_str(&serde_json::to_string(event).expect("serialize event"));
        actual_events.push('\n');
    }
    let expected_events =
        std::fs::read_to_string(fixtures_dir().join(scenario).join("events.jsonl"))
            .expect("read fixture events");
    diff_jsonl(
        &prepare_event_lines(&expected_events),
        &prepare_event_lines(&actual_events),
    )
    .unwrap_or_else(|f| panic!("{scenario}: event parity diff:\n{f}"));
}

// ---------------------------------------------------------------------------
// Scenarios (scripts ported one-by-one from generate-fixtures.mjs)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parity_single_turn() {
    async fn scenario() {
        let run = start_scenario(
            "single-turn",
            vec![faux_assistant_message(
                "Hello from the faux provider!",
                FauxAssistantOptions::default(),
            )
            .into()],
            FauxProviderOptions::default(),
            |_| {},
        )
        .await;
        run.session
            .prompt("Say hello.", PromptOptions::default())
            .await
            .expect("prompt");
        run.session.wait_for_idle().await;
        compare_with_fixture("single-turn", &run);
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parity_tool_calls() {
    async fn scenario() {
        let run = start_scenario(
            "tool-calls",
            vec![
                faux_assistant_message(
                    vec![
                        faux_text("Let me read the note and run a command."),
                        faux_tool_call(
                            "read",
                            json!({ "path": "note.txt" }).as_object().unwrap().clone(),
                            Some("fixture-tool-call-1".to_owned()),
                        ),
                        faux_tool_call(
                            "bash",
                            json!({ "command": "echo fixture-bash-output" })
                                .as_object()
                                .unwrap()
                                .clone(),
                            Some("fixture-tool-call-2".to_owned()),
                        ),
                    ],
                    FauxAssistantOptions {
                        stop_reason: Some(StopReason::ToolUse),
                        ..Default::default()
                    },
                )
                .into(),
                faux_assistant_message(
                    "I read the note and ran the command.",
                    FauxAssistantOptions::default(),
                )
                .into(),
            ],
            FauxProviderOptions::default(),
            |cwd| {
                std::fs::write(cwd.join("note.txt"), "fixture note content\n").expect("plant note");
            },
        )
        .await;
        run.session
            .prompt(
                "Read note.txt and run the echo command.",
                PromptOptions::default(),
            )
            .await
            .expect("prompt");
        run.session.wait_for_idle().await;
        compare_with_fixture("tool-calls", &run);
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parity_steering_followup() {
    async fn scenario() {
        let run = start_scenario(
            "steering-followup",
            vec![
                faux_assistant_message(
                    "This is a long first answer that keeps streaming so a steering message can interrupt it mid-turn. ".repeat(8),
                    FauxAssistantOptions::default(),
                )
                .into(),
                faux_assistant_message("Answer after the steering interruption.", FauxAssistantOptions::default())
                    .into(),
                faux_assistant_message(
                    "Second answer, also long enough that a follow-up message can be queued while it streams. ".repeat(8),
                    FauxAssistantOptions::default(),
                )
                .into(),
                faux_assistant_message("Final answer after steering and follow-up.", FauxAssistantOptions::default())
                    .into(),
            ],
            FauxProviderOptions {
                tokens_per_second: Some(50.0),
                ..Default::default()
            },
            |_| {},
        )
        .await;

        let events = run.events.clone();
        let session = run.session.clone();
        let mark = event_count(&events);
        let first = tokio::spawn(async move {
            session
                .prompt("Start the first answer.", Default::default())
                .await
        });
        wait_for_message_update(&events, mark).await;
        run.session
            .steer("Change of plans: answer briefly.", None)
            .await
            .expect("steer");
        first
            .await
            .expect("first prompt task")
            .expect("first prompt");

        let session = run.session.clone();
        let mark = event_count(&events);
        let second = tokio::spawn(async move {
            session
                .prompt("Now the second answer.", Default::default())
                .await
        });
        wait_for_message_update(&events, mark).await;
        run.session
            .follow_up("And one more thing.", None)
            .await
            .expect("follow_up");
        second
            .await
            .expect("second prompt task")
            .expect("second prompt");
        run.session.wait_for_idle().await;

        compare_with_fixture("steering-followup", &run);
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parity_abort() {
    async fn scenario() {
        let run = start_scenario(
            "abort",
            vec![faux_assistant_message(
                "A long answer that the user will abort before it finishes streaming. ".repeat(8),
                FauxAssistantOptions::default(),
            )
            .into()],
            FauxProviderOptions {
                tokens_per_second: Some(50.0),
                ..Default::default()
            },
            |_| {},
        )
        .await;

        let events = run.events.clone();
        let session = run.session.clone();
        let mark = event_count(&events);
        let pending = tokio::spawn(async move {
            session
                .prompt("Give me a long answer.", Default::default())
                .await
        });
        wait_for_message_update(&events, mark).await;
        run.session.abort().await;
        pending.await.expect("prompt task").expect("prompt");
        run.session.wait_for_idle().await;

        compare_with_fixture("abort", &run);
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parity_length_truncation() {
    async fn scenario() {
        let run = start_scenario(
            "length-truncation",
            vec![faux_assistant_message(
                "Truncated answer that hit the max token limit",
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::Length),
                    ..Default::default()
                },
            )
            .into()],
            FauxProviderOptions::default(),
            |_| {},
        )
        .await;
        run.session
            .prompt(
                "Answer until you run out of tokens.",
                PromptOptions::default(),
            )
            .await
            .expect("prompt");
        run.session.wait_for_idle().await;
        compare_with_fixture("length-truncation", &run);
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

// ---------------------------------------------------------------------------
// print / json modes (driven directly via run_print_mode)
// ---------------------------------------------------------------------------

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

/// print mode (text): prints the last assistant's text block, exit 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn print_mode_text_output() {
    let (mut runtime, _tmp) = build_runtime(
        "print-text",
        vec![faux_assistant_message(
            "Hello from the faux provider!",
            FauxAssistantOptions::default(),
        )
        .into()],
    )
    .await;
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let exit = run_print_mode(
        &mut runtime,
        PrintModeOptions {
            mode: PrintOutputMode::Text,
            messages: Vec::new(),
            initial_message: Some("Say hello.".to_owned()),
            initial_images: None,
        },
        &mut out,
        &mut err,
    )
    .await;
    assert_eq!(exit, 0, "stderr: {}", String::from_utf8_lossy(&err));
    assert_eq!(
        String::from_utf8(out).expect("utf8 out"),
        "Hello from the faux provider!\n"
    );
}

/// json mode: session header line + event sequence (agent_start → agent_settled).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn print_mode_json_event_stream() {
    let (mut runtime, _tmp) = build_runtime(
        "print-json",
        vec![faux_assistant_message(
            "Hello from the faux provider!",
            FauxAssistantOptions::default(),
        )
        .into()],
    )
    .await;
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let exit = run_print_mode(
        &mut runtime,
        PrintModeOptions {
            mode: PrintOutputMode::Json,
            messages: Vec::new(),
            initial_message: Some("Say hello.".to_owned()),
            initial_images: None,
        },
        &mut out,
        &mut err,
    )
    .await;
    assert_eq!(exit, 0, "stderr: {}", String::from_utf8_lossy(&err));
    let out = String::from_utf8(out).expect("utf8 out");
    let lines: Vec<Value> = out
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect();
    // First line is the session header (print-mode.ts:112-117).
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["version"], 3);
    assert!(lines[0]["id"].as_str().is_some());
    assert!(lines[0]["cwd"].as_str().is_some());
    let event_types: Vec<&str> = lines[1..]
        .iter()
        .map(|line| line["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(event_types.first(), Some(&"agent_start"));
    assert_eq!(event_types.last(), Some(&"agent_settled"));
    for expected in [
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(
            event_types.contains(&expected),
            "missing {expected}: {event_types:?}"
        );
    }
}

/// print mode: multiple messages sent in order (print-mode.ts:120-128); prints the last
/// round's assistant text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn print_mode_sends_all_messages_in_order() {
    let (mut runtime, _tmp) = build_runtime(
        "print-multi",
        vec![
            faux_assistant_message("answer one", FauxAssistantOptions::default()).into(),
            faux_assistant_message("answer two", FauxAssistantOptions::default()).into(),
            faux_assistant_message("answer three", FauxAssistantOptions::default()).into(),
        ],
    )
    .await;
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let exit = run_print_mode(
        &mut runtime,
        PrintModeOptions {
            mode: PrintOutputMode::Text,
            messages: vec!["second".to_owned(), "third".to_owned()],
            initial_message: Some("first".to_owned()),
            initial_images: None,
        },
        &mut out,
        &mut err,
    )
    .await;
    assert_eq!(exit, 0, "stderr: {}", String::from_utf8_lossy(&err));
    assert_eq!(String::from_utf8(out).expect("utf8 out"), "answer three\n");
    let user_texts: Vec<String> = runtime
        .session()
        .messages()
        .iter()
        .filter_map(|m| match m {
            rpi_agent::messages::AgentMessage::User(user) => {
                Some(rpi_ai::utils::text::content_text_user(&user.content, ""))
            }
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["first", "second", "third"]);
}

/// print mode: error / aborted final assistant → stderr + exit 1
///（print-mode.ts:129-146）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn print_mode_error_and_aborted_exit_1() {
    let (mut runtime, _tmp) = build_runtime(
        "print-error",
        vec![faux_assistant_message(
            "",
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("provider exploded".to_owned()),
                ..Default::default()
            },
        )
        .into()],
    )
    .await;
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let exit = run_print_mode(
        &mut runtime,
        PrintModeOptions {
            mode: PrintOutputMode::Text,
            messages: Vec::new(),
            initial_message: Some("boom?".to_owned()),
            initial_images: None,
        },
        &mut out,
        &mut err,
    )
    .await;
    assert_eq!(exit, 1);
    assert!(String::from_utf8_lossy(&err).contains("provider exploded"));

    let (mut runtime, _tmp) = build_runtime(
        "print-aborted",
        vec![faux_assistant_message(
            "",
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Aborted),
                error_message: Some("Request was aborted".to_owned()),
                ..Default::default()
            },
        )
        .into()],
    )
    .await;
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let exit = run_print_mode(
        &mut runtime,
        PrintModeOptions {
            mode: PrintOutputMode::Text,
            messages: Vec::new(),
            initial_message: Some("abort?".to_owned()),
            initial_images: None,
        },
        &mut out,
        &mut err,
    )
    .await;
    assert_eq!(exit, 1);
    assert!(String::from_utf8_lossy(&err).contains("Request was aborted"));
}
