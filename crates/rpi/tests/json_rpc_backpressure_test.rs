//! T18 stdout-backpressure integration tests (print `--mode json` and RPC).
//!
//! Upstream anchors: `core/output-guard.ts` @ 4181f66 (`writeRawStdout` +
//! `waitForRawStdoutBackpressure`), `print-mode.ts:108-118`,
//! `rpc-mode.ts:355-363`. The rpi mapping (see `core::output_guard`): a
//! single blocking writer; producers stall while the consumer's pipe is
//! full — no event is dropped, merged, or buffered without bound.
//!
//! Both tests use a *gated* writer: while the gate is closed the consumer
//! side never drains, so any unbounded buffer in the write path would show
//! up as output bytes appearing before the gate opens, and any deadlock
//! shows up as a timeout after it opens.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use rpi::core::agent_session_runtime::{
    create_agent_session_runtime, AgentSessionRuntime, CreateAgentSessionRuntimeFactory,
    CreateAgentSessionRuntimeResult, CreateRuntimeOptions,
};
use rpi::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
use rpi::core::output_guard::RawStdout;
use rpi::core::session_manager::{NewSessionOptions, SessionManager};
use rpi::modes::print_mode::{run_print_mode, PrintModeOptions, PrintOutputMode};
use rpi::modes::rpc::run_rpc_mode;
use rpi_test_support::faux::{
    faux_assistant_message, FauxAiProvider, FauxAssistantOptions, FauxModelDefinition,
    FauxProvider, FauxProviderOptions, FauxResponseStep,
};

const DEADLINE: Duration = Duration::from_secs(15);

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
            "rpi-backpressure-{name}-{}-{nanos}-{id}",
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

/// Stdout test double: appends to a shared buffer, but only once the gate
/// is open — a closed gate models a consumer that never reads (full pipe).
#[derive(Clone, Default)]
struct GatedBuf {
    inner: Arc<Mutex<Vec<u8>>>,
    open: Arc<AtomicBool>,
}

impl GatedBuf {
    fn bytes(&self) -> Vec<u8> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn open_gate(&self) {
        self.open.store(true, Ordering::SeqCst);
    }
}

impl Write for GatedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        while !self.open.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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

/// Parse captured stdout bytes into JSON lines; assert the delta-only
/// `message_update` wire shape; return the event `type` sequence.
fn assert_wire_lines(bytes: &[u8], skip_header: bool) -> (Vec<Value>, Vec<String>) {
    let text = String::from_utf8(bytes.to_vec()).expect("utf8 stdout");
    let mut lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect();
    if skip_header {
        assert_eq!(lines[0]["type"], "session", "first line is the header");
        lines.remove(0);
    }
    let mut types = Vec::new();
    for line in &lines {
        let event_type = line["type"].as_str().expect("event type").to_owned();
        if event_type == "message_update" {
            assert!(
                line.get("message").is_none(),
                "wire message_update must not carry `message`: {line}"
            );
            let delta_event = &line["assistantMessageEvent"];
            assert!(
                delta_event.get("partial").is_none(),
                "wire message_update must not carry `partial`: {line}"
            );
            if delta_event["type"].as_str() == Some("text_delta") {
                assert!(delta_event.get("contentIndex").is_some());
                assert!(delta_event.get("delta").is_some());
            }
        }
        types.push(event_type);
    }
    (lines, types)
}

/// print `--mode json`: with the consumer stalled, nothing is written and
/// nothing piles up; once the gate opens the complete ordered event stream
/// arrives and delta assembly reproduces `message_end.message`
/// (docs/json.md:82-85 consistency check on the real stdout path).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn print_json_mode_backpressure_slow_consumer() {
    let text = "backpressure ".repeat(300);
    let (mut runtime, _tmp) = build_runtime(
        "print",
        vec![faux_assistant_message(text.as_str(), FauxAssistantOptions::default()).into()],
    )
    .await;
    let out = GatedBuf::default();

    // run_print_mode's future borrows `&mut dyn Write` (non-Send), so drive
    // it on a dedicated thread with its own current-thread runtime.
    let thread_out = out.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("thread runtime");
        let exit = runtime_rt.block_on(async move {
            let mut err: Vec<u8> = Vec::new();
            run_print_mode(
                &mut runtime,
                PrintModeOptions {
                    mode: PrintOutputMode::Json,
                    messages: Vec::new(),
                    initial_message: Some("stream text".to_owned()),
                    initial_images: None,
                },
                RawStdout::new(Box::new(thread_out)),
                &mut err,
            )
            .await
        });
        let _ = done_tx.send(exit);
    });

    // Consumer stalled: no bytes may appear (any unbounded staging buffer
    // would fill while the gate stays closed).
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        out.bytes().is_empty(),
        "no output may be staged while the consumer is stalled: {:?}",
        String::from_utf8_lossy(&out.bytes())
    );

    out.open_gate();
    let exit = done_rx
        .recv_timeout(DEADLINE)
        .expect("print mode finishes once the consumer drains");
    assert_eq!(exit, 0);

    let (lines, types) = assert_wire_lines(&out.bytes(), true);
    assert_eq!(types.first().map(String::as_str), Some("agent_start"));
    assert_eq!(types.last().map(String::as_str), Some("agent_settled"));
    assert!(types.iter().any(|t| t == "message_update"));

    // Delta assembly must reproduce the authoritative `message_end.message`.
    let mut assembled = String::new();
    let mut final_text: Option<String> = None;
    for line in &lines {
        match line["type"].as_str() {
            Some("message_update")
                if line["assistantMessageEvent"]["type"].as_str() == Some("text_delta") =>
            {
                assembled.push_str(
                    line["assistantMessageEvent"]["delta"]
                        .as_str()
                        .expect("delta"),
                );
            }
            Some("message_end") => {
                final_text = Some(
                    line["message"]["content"][0]["text"]
                        .as_str()
                        .expect("final text")
                        .to_owned(),
                );
            }
            _ => {}
        }
    }
    assert_eq!(assembled, final_text.expect("message_end emitted"));
    assert_eq!(assembled, text);
}

/// RPC mode: same contract over the RPC event stream — stalled consumer ⇒
/// no staged bytes and no lost/reordered events once it drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_mode_backpressure_slow_consumer() {
    let text = "rpc backpressure ".repeat(200);
    let (runtime, _tmp) = build_runtime(
        "rpc",
        vec![faux_assistant_message(text.as_str(), FauxAssistantOptions::default()).into()],
    )
    .await;
    let out = GatedBuf::default();

    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    // The read half must drop immediately: the server observes stdin EOF
    // only once *both* client halves are gone (rpc_mode_test's `boot` gets
    // this by scoping its `_read_half` to the constructor).
    let (_, mut stdin) = tokio::io::split(client_io);
    let server_io = tokio::io::BufReader::new(server_io);
    let writer = out.clone();
    let handle =
        tokio::spawn(async move { run_rpc_mode(runtime, server_io, Box::new(writer)).await });

    stdin
        .write_all(b"{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"stream text\"}\n")
        .await
        .expect("write prompt");
    stdin.flush().await.expect("flush prompt");

    // Consumer stalled: neither the acceptance response nor any event may
    // be staged into an intermediate buffer.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        out.bytes().is_empty(),
        "no output may be staged while the consumer is stalled: {:?}",
        String::from_utf8_lossy(&out.bytes())
    );

    out.open_gate();

    // Read until agent_settled (deadline-bounded poll of the capture).
    let deadline = Instant::now() + DEADLINE;
    let lines = loop {
        let bytes = out.bytes();
        let text = String::from_utf8(bytes).expect("utf8 stdout");
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect();
        if lines
            .iter()
            .any(|line| line["type"].as_str() == Some("agent_settled"))
        {
            break lines;
        }
        assert!(
            Instant::now() < deadline,
            "timeout waiting for agent_settled; got:\n{}",
            String::from_utf8_lossy(&out.bytes())
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };

    // Prompt acceptance response made it out exactly once.
    let responses: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"].as_str() == Some("response"))
        .collect();
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0],
        &json!({"id": "p1", "type": "response", "command": "prompt", "success": true})
    );

    // Event order and delta-only shape on the wire.
    let events: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"].as_str() != Some("response"))
        .collect();
    let mut assembled = String::new();
    let mut final_text: Option<String> = None;
    let mut types = Vec::new();
    for event in &events {
        let event_type = event["type"].as_str().expect("event type");
        types.push(event_type.to_owned());
        match event_type {
            "message_update" => {
                assert!(event.get("message").is_none(), "delta-only: {event}");
                let delta_event = &event["assistantMessageEvent"];
                assert!(delta_event.get("partial").is_none(), "delta-only: {event}");
                if delta_event["type"].as_str() == Some("text_delta") {
                    assembled.push_str(delta_event["delta"].as_str().expect("delta"));
                }
            }
            "message_end" => {
                final_text = Some(
                    event["message"]["content"][0]["text"]
                        .as_str()
                        .expect("final text")
                        .to_owned(),
                );
            }
            _ => {}
        }
    }
    assert_eq!(types.first().map(String::as_str), Some("agent_start"));
    assert_eq!(types.last().map(String::as_str), Some("agent_settled"));
    assert!(types.iter().any(|t| t == "message_update"));
    assert_eq!(assembled, final_text.expect("message_end emitted"));
    assert_eq!(assembled, text);

    // Clean shutdown on stdin EOF.
    drop(stdin);
    let exit = tokio::time::timeout(DEADLINE, handle)
        .await
        .expect("rpc shutdown timed out")
        .expect("rpc task panicked");
    assert_eq!(exit, 0);
}
