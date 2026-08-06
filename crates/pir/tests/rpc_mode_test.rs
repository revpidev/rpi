//! RPC 模式 32 命令契约测试（`docs/rpc.md` 逐条对拍基准）+ 帧格式/错误路径。
//!
//! 驱动方式：经 `run_rpc_mode` 全栈运行（真实 `AgentSessionRuntime` +
//! FauxProvider 脚本化响应），客户端用 duplex 流逐条下发命令并读取响应，
//! 与 rpc.md 的线协议逐条核对。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use pir::core::agent_session_runtime::{
    create_agent_session_runtime, CreateAgentSessionRuntimeFactory,
    CreateAgentSessionRuntimeResult, CreateRuntimeOptions,
};
use pir::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use pir::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
use pir::core::session_manager::{NewSessionOptions, SessionManager};
use pir::modes::rpc::run_rpc_mode;
use pir_agent::messages::AgentMessage;
use pir_ai::types::Model;
use pir_test_support::faux::{
    faux_assistant_message, FauxAiProvider, FauxAssistantOptions, FauxModelDefinition,
    FauxProvider, FauxProviderOptions, FauxResponseStep,
};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pir-rpc-test-{}-{nanos}-{id}", std::process::id()));
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

/// Shared output capture (`Box<dyn Write + Send>` for `run_rpc_mode`).
#[derive(Clone, Default)]
struct SharedBuf {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl SharedBuf {
    fn bytes(&self) -> Vec<u8> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
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

fn assistant(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

/// Two-model default catalog: `faux-1` (reasoning), `faux-2` (plain).
fn default_models() -> Vec<FauxModelDefinition> {
    vec![
        FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: Some("Faux One".to_owned()),
            reasoning: Some(true),
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        },
        FauxModelDefinition {
            id: "faux-2".to_owned(),
            name: Some("Faux Two".to_owned()),
            reasoning: Some(false),
            input: None,
            cost: None,
            context_window: Some(100_000),
            max_tokens: Some(4096),
        },
    ]
}

struct RpcSession {
    provider: Arc<FauxProvider>,
    tmp: TempDir,
    cwd: PathBuf,
    stdin: Option<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    buf: SharedBuf,
    cursor: usize,
    handle: Option<tokio::task::JoinHandle<i32>>,
}

impl RpcSession {
    async fn send(&mut self, command: &Value) {
        let mut line = serde_json::to_string(command).expect("serialize command");
        line.push('\n');
        self.send_raw(&line).await;
    }

    async fn send_raw(&mut self, raw: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(raw.as_bytes()).await.expect("write raw");
        stdin.flush().await.expect("flush raw");
    }

    /// Next stdout line as JSON (15s deadline).
    async fn next_value(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            {
                let buffered = self.buf.bytes();
                if let Some(pos) = buffered[self.cursor..].iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8(buffered[self.cursor..self.cursor + pos].to_vec())
                        .expect("utf8 line");
                    self.cursor += pos + 1;
                    return serde_json::from_str(&line).expect("json line");
                }
            }
            assert!(
                Instant::now() < deadline,
                "timeout waiting for rpc line; buffered so far:\n{}",
                String::from_utf8_lossy(&self.buf.bytes())
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Next response (skipping events), optionally matched by id.
    async fn next_response(&mut self, id: Option<&str>) -> Value {
        self.next_response_with_events(id).await.0
    }

    /// Like [`next_response`], but also returns the events seen on the way
    /// (events and responses interleave on one stream; callers that assert
    /// on both must not discard them).
    async fn next_response_with_events(&mut self, id: Option<&str>) -> (Value, Vec<Value>) {
        let mut events = Vec::new();
        loop {
            let value = self.next_value().await;
            if value.get("type").and_then(Value::as_str) != Some("response") {
                events.push(value);
                continue;
            }
            if let Some(id) = id {
                if value.get("id").and_then(Value::as_str) != Some(id) {
                    events.push(value);
                    continue;
                }
            }
            return (value, events);
        }
    }

    /// Next event of the given `type` (skipping everything else).
    async fn next_event(&mut self, event_type: &str) -> Value {
        loop {
            let value = self.next_value().await;
            if value.get("type").and_then(Value::as_str) == Some(event_type) {
                return value;
            }
        }
    }

    /// Close stdin (EOF) and await shutdown; buffered output stays readable.
    async fn close_and_wait(&mut self) -> i32 {
        drop(self.stdin.take());
        let handle = self.handle.take().expect("handle");
        tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("rpc shutdown timed out")
            .expect("rpc task panicked")
    }
}

/// Boot a full RPC session in `tmp`: real runtime + FauxProvider, in-memory
/// session, duplex stdin, captured stdout. `tmp/agent` and `tmp/cwd` must
/// already exist (callers may plant resources in them first).
async fn boot(
    tmp: TempDir,
    provider_options: FauxProviderOptions,
    responses: Vec<FauxResponseStep>,
) -> RpcSession {
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let provider = FauxProvider::new(provider_options);
    provider.set_responses(responses);
    let model: Model = provider.get_model(None).expect("faux model");

    let model_runtime = pir::core::model_runtime::ModelRuntime::create(CreateModelRuntimeOptions {
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
                let created = pir::sdk::create_agent_session(pir::sdk::CreateAgentSessionOptions {
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
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_manager,
            session_start_event: None,
        },
    )
    .await
    .expect("create runtime");

    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (_read_half, stdin) = tokio::io::split(client_io);
    let server_io = tokio::io::BufReader::new(server_io);
    let buf = SharedBuf::default();
    let writer_buf = buf.clone();
    let handle =
        tokio::spawn(async move { run_rpc_mode(runtime, server_io, Box::new(writer_buf)).await });

    RpcSession {
        provider,
        tmp,
        cwd,
        stdin: Some(stdin),
        buf,
        cursor: 0,
        handle: Some(handle),
    }
}

/// Default boot: two-model catalog, fresh temp dirs.
async fn start_rpc(responses: Vec<FauxResponseStep>) -> RpcSession {
    start_rpc_with(FauxProviderOptions::default(), responses).await
}

async fn start_rpc_with(
    provider_options: FauxProviderOptions,
    responses: Vec<FauxResponseStep>,
) -> RpcSession {
    let mut options = provider_options;
    if options.models.is_none() {
        options.models = Some(default_models());
    }
    boot(TempDir::new(), options, responses).await
}

// ---------------------------------------------------------------------------
// Prompt lifecycle + messages + state + stats
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_lifecycle_messages_state_stats() {
    let mut rpc = start_rpc(vec![assistant("Hello from faux!")]).await;

    // prompt: acceptance response, then the event stream.
    rpc.send(&json!({"id": "p1", "type": "prompt", "message": "hi"}))
        .await;
    let response = rpc.next_response(Some("p1")).await;
    assert_eq!(
        response,
        json!({"id": "p1", "type": "response", "command": "prompt", "success": true})
    );

    // Event sequence (rpc.md §Events).
    let mut event_types = Vec::new();
    loop {
        let event = rpc.next_value().await;
        let event_type = event["type"].as_str().expect("event type").to_owned();
        let done = event_type == "agent_settled";
        event_types.push(event_type);
        if done {
            break;
        }
    }
    for expected in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
        "agent_settled",
    ] {
        assert!(
            event_types.iter().any(|t| t == expected),
            "missing event {expected} in {event_types:?}"
        );
    }
    assert_eq!(event_types.first().map(String::as_str), Some("agent_start"));
    assert_eq!(
        event_types.last().map(String::as_str),
        Some("agent_settled")
    );

    // The scripted response was consumed exactly once.
    assert_eq!(rpc.provider.call_count(), 1);

    // get_last_assistant_text
    rpc.send(&json!({"id": "t1", "type": "get_last_assistant_text"}))
        .await;
    let response = rpc.next_response(Some("t1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["text"], "Hello from faux!");

    // get_messages: user + assistant
    rpc.send(&json!({"id": "m1", "type": "get_messages"})).await;
    let response = rpc.next_response(Some("m1")).await;
    let messages = response["data"]["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");

    // get_state: full RpcSessionState shape (rpc.md §get_state).
    rpc.send(&json!({"id": "s1", "type": "get_state"})).await;
    let response = rpc.next_response(Some("s1")).await;
    let data = &response["data"];
    assert_eq!(data["model"]["id"], "faux-1");
    assert_eq!(data["model"]["provider"], "faux");
    assert_eq!(data["thinkingLevel"], "medium");
    assert_eq!(data["isStreaming"], false);
    assert_eq!(data["isCompacting"], false);
    assert_eq!(data["steeringMode"], "one-at-a-time");
    assert_eq!(data["followUpMode"], "one-at-a-time");
    assert!(data["sessionId"].as_str().is_some());
    assert_eq!(data["autoCompactionEnabled"], true);
    assert_eq!(data["messageCount"], 2);
    assert_eq!(data["pendingMessageCount"], 0);
    // in-memory session: no sessionFile / sessionName keys.
    assert!(data.get("sessionFile").is_none());
    assert!(data.get("sessionName").is_none());

    // get_session_stats (rpc.md §get_session_stats).
    rpc.send(&json!({"id": "st1", "type": "get_session_stats"}))
        .await;
    let response = rpc.next_response(Some("st1")).await;
    let data = &response["data"];
    assert_eq!(data["userMessages"], 1);
    assert_eq!(data["assistantMessages"], 1);
    assert_eq!(data["totalMessages"], 2);
    assert!(data["tokens"]["input"].as_u64().is_some());
    assert!(data["cost"].as_f64().is_some());
    assert!(data["contextUsage"]["contextWindow"].as_u64().is_some());

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// Model + thinking commands
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_and_thinking_commands() {
    let mut rpc = start_rpc(vec![]).await;

    // get_available_models
    rpc.send(&json!({"id": "a1", "type": "get_available_models"}))
        .await;
    let response = rpc.next_response(Some("a1")).await;
    let models = response["data"]["models"].as_array().expect("models");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"], "faux-1");
    assert_eq!(models[1]["id"], "faux-2");

    // set_model (ok): data is the full Model object.
    rpc.send(&json!({"id": "sm1", "type": "set_model", "provider": "faux", "modelId": "faux-2"}))
        .await;
    let response = rpc.next_response(Some("sm1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["id"], "faux-2");
    assert_eq!(response["data"]["provider"], "faux");

    // set_model (unknown): `Model not found: provider/id` (rpc-mode.ts:470).
    rpc.send(&json!({"id": "sm2", "type": "set_model", "provider": "faux", "modelId": "nope"}))
        .await;
    let response = rpc.next_response(Some("sm2")).await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"], "Model not found: faux/nope");

    // cycle_model: 2 models → next one (currently faux-2 → faux-1), not scoped.
    rpc.send(&json!({"id": "cm1", "type": "cycle_model"})).await;
    let response = rpc.next_response(Some("cm1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["model"]["id"], "faux-1");
    assert_eq!(response["data"]["isScoped"], false);
    assert!(response["data"]["thinkingLevel"].as_str().is_some());

    // get_available_thinking_levels (reasoning model faux-1).
    rpc.send(&json!({"id": "tl1", "type": "get_available_thinking_levels"}))
        .await;
    let response = rpc.next_response(Some("tl1")).await;
    assert_eq!(
        response["data"]["levels"],
        json!(["off", "minimal", "low", "medium", "high"])
    );

    // set_thinking_level + thinking_level_changed event.
    rpc.send(&json!({"id": "tl2", "type": "set_thinking_level", "level": "high"}))
        .await;
    let (response, events) = rpc.next_response_with_events(Some("tl2")).await;
    assert_eq!(response["success"], true);
    let event = events
        .iter()
        .find(|e| e["type"] == "thinking_level_changed")
        .expect("thinking_level_changed event");
    assert_eq!(event["level"], "high");

    // cycle_thinking_level: high wraps to off.
    rpc.send(&json!({"id": "tl3", "type": "cycle_thinking_level"}))
        .await;
    let response = rpc.next_response(Some("tl3")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["level"], "off");

    assert_eq!(rpc.close_and_wait().await, 0);
}

/// cycle_model / cycle_thinking_level 的单模型/无推理 null data 路径
/// (rpc.md §cycle_model §cycle_thinking_level)。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cycle_commands_null_data_paths() {
    let mut rpc = start_rpc_with(
        FauxProviderOptions {
            models: Some(vec![FauxModelDefinition {
                id: "faux-1".to_owned(),
                name: None,
                reasoning: Some(false),
                input: None,
                cost: None,
                context_window: Some(8192),
                max_tokens: Some(4096),
            }]),
            ..Default::default()
        },
        vec![],
    )
    .await;

    rpc.send(&json!({"id": "cm1", "type": "cycle_model"})).await;
    let response = rpc.next_response(Some("cm1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"], Value::Null);

    rpc.send(&json!({"id": "ct1", "type": "cycle_thinking_level"}))
        .await;
    let response = rpc.next_response(Some("ct1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"], Value::Null);

    // 非推理模型的可用级别只有 off（rpc.md §get_available_thinking_levels）。
    rpc.send(&json!({"id": "tl1", "type": "get_available_thinking_levels"}))
        .await;
    let response = rpc.next_response(Some("tl1")).await;
    assert_eq!(response["data"]["levels"], json!(["off"]));

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// Queue modes / compaction & retry toggles
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_mode_and_toggle_commands() {
    let mut rpc = start_rpc(vec![]).await;

    rpc.send(&json!({"id": "q1", "type": "set_steering_mode", "mode": "all"}))
        .await;
    assert_eq!(rpc.next_response(Some("q1")).await["success"], true);

    rpc.send(&json!({"id": "q2", "type": "set_follow_up_mode", "mode": "all"}))
        .await;
    assert_eq!(rpc.next_response(Some("q2")).await["success"], true);

    rpc.send(&json!({"id": "q3", "type": "set_auto_compaction", "enabled": false}))
        .await;
    assert_eq!(rpc.next_response(Some("q3")).await["success"], true);

    rpc.send(&json!({"id": "q4", "type": "set_auto_retry", "enabled": false}))
        .await;
    assert_eq!(rpc.next_response(Some("q4")).await["success"], true);

    rpc.send(&json!({"id": "q5", "type": "abort_retry"})).await;
    assert_eq!(rpc.next_response(Some("q5")).await["success"], true);

    rpc.send(&json!({"id": "q6", "type": "get_state"})).await;
    let data = rpc.next_response(Some("q6")).await;
    assert_eq!(data["data"]["steeringMode"], "all");
    assert_eq!(data["data"]["followUpMode"], "all");
    assert_eq!(data["data"]["autoCompactionEnabled"], false);

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// Bash commands
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_commands() {
    let mut rpc = start_rpc(vec![]).await;

    // bash with id: bash_execution_update events carry the command id
    // (rpc.md §bash_execution_update).
    rpc.send(&json!({"id": "b1", "type": "bash", "command": "printf 'hello-rpc'"}))
        .await;
    let update = rpc.next_event("bash_execution_update").await;
    assert_eq!(update["id"], "b1");
    assert!(update["delta"]
        .as_str()
        .expect("delta")
        .contains("hello-rpc"));
    let response = rpc.next_response(Some("b1")).await;
    let data = &response["data"];
    assert_eq!(data["output"], "hello-rpc");
    assert_eq!(data["exitCode"], 0);
    assert_eq!(data["cancelled"], false);
    assert_eq!(data["truncated"], false);
    assert!(data.get("fullOutputPath").is_none());

    // bashExecution message landed in the message state (rpc.md §bash).
    rpc.send(&json!({"id": "b2", "type": "get_messages"})).await;
    let response = rpc.next_response(Some("b2")).await;
    let messages = response["data"]["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "bashExecution");
    assert_eq!(messages[0]["command"], "printf 'hello-rpc'");
    assert_eq!(messages[0]["output"], "hello-rpc");

    // abort_bash with nothing running: success no-op.
    rpc.send(&json!({"id": "b3", "type": "abort_bash"})).await;
    assert_eq!(rpc.next_response(Some("b3")).await["success"], true);

    assert_eq!(rpc.close_and_wait().await, 0);
}

/// bash 运行中可被 abort_bash 取消（bash/abort_bash 往返）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_abort_roundtrip() {
    let mut rpc = start_rpc(vec![]).await;

    rpc.send(&json!({"id": "b1", "type": "bash", "command": "sleep 30"}))
        .await;
    // 给 executor 一点启动时间，再发 abort。
    tokio::time::sleep(Duration::from_millis(150)).await;
    rpc.send(&json!({"id": "b2", "type": "abort_bash"})).await;
    assert_eq!(rpc.next_response(Some("b2")).await["success"], true);
    let response = rpc.next_response(Some("b1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["cancelled"], true);

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// Entries / tree / fork messages
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn entries_tree_fork_messages() {
    let mut rpc = start_rpc(vec![assistant("answer one")]).await;
    rpc.send(&json!({"id": "p1", "type": "prompt", "message": "question one"}))
        .await;
    rpc.next_response(Some("p1")).await;
    rpc.next_event("agent_settled").await;

    // get_fork_messages (rpc.md §get_fork_messages).
    rpc.send(&json!({"id": "f1", "type": "get_fork_messages"}))
        .await;
    let response = rpc.next_response(Some("f1")).await;
    let fork_messages = response["data"]["messages"].as_array().expect("messages");
    assert_eq!(fork_messages.len(), 1);
    assert_eq!(fork_messages[0]["text"], "question one");
    let fork_entry_id = fork_messages[0]["entryId"]
        .as_str()
        .expect("entryId")
        .to_owned();

    // get_entries: full list + leafId.
    rpc.send(&json!({"id": "e1", "type": "get_entries"})).await;
    let response = rpc.next_response(Some("e1")).await;
    let entries = response["data"]["entries"].as_array().expect("entries");
    assert!(entries.len() >= 3, "entries: {entries:?}");
    let first_id = entries[0]["id"].as_str().expect("entry id").to_owned();
    let leaf_id = response["data"]["leafId"]
        .as_str()
        .expect("leafId")
        .to_owned();
    assert_eq!(
        leaf_id,
        entries.last().expect("last")["id"]
            .as_str()
            .expect("last id")
    );

    // get_entries with since cursor: strictly after the given id.
    rpc.send(&json!({"id": "e2", "type": "get_entries", "since": first_id}))
        .await;
    let response = rpc.next_response(Some("e2")).await;
    let remaining = response["data"]["entries"].as_array().expect("entries");
    assert_eq!(remaining.len(), entries.len() - 1);

    // get_entries with unknown since: error (rpc.md §get_entries).
    rpc.send(&json!({"id": "e3", "type": "get_entries", "since": "nope"}))
        .await;
    let response = rpc.next_response(Some("e3")).await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"], "Entry not found: nope");

    // get_tree: {entry, children} nodes + leafId.
    rpc.send(&json!({"id": "e4", "type": "get_tree"})).await;
    let response = rpc.next_response(Some("e4")).await;
    let tree = response["data"]["tree"].as_array().expect("tree");
    assert_eq!(tree.len(), 1, "single root expected: {tree:?}");
    assert!(tree[0]["entry"]["id"].as_str().is_some());
    assert!(tree[0]["children"].as_array().is_some());
    assert_eq!(response["data"]["leafId"], leaf_id);

    // fork from the user message (rpc.md §fork).
    rpc.send(&json!({"id": "f2", "type": "fork", "entryId": fork_entry_id}))
        .await;
    let response = rpc.next_response(Some("f2")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["text"], "question one");
    assert_eq!(response["data"]["cancelled"], false);

    // After fork-before the user message: session replaced, empty branch.
    rpc.send(&json!({"id": "f3", "type": "get_last_assistant_text"}))
        .await;
    let response = rpc.next_response(Some("f3")).await;
    assert_eq!(response["data"]["text"], Value::Null);

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// Session replacement: new_session / clone / switch_session
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_replacement_commands() {
    let mut rpc = start_rpc(vec![assistant("first answer")]).await;

    // One prompt so clone has a message-bearing leaf.
    rpc.send(&json!({"id": "p1", "type": "prompt", "message": "hello"}))
        .await;
    rpc.next_response(Some("p1")).await;
    rpc.next_event("agent_settled").await;

    // clone: duplicate the active branch at the current position.
    rpc.send(&json!({"id": "c1", "type": "clone"})).await;
    let response = rpc.next_response(Some("c1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["cancelled"], false);

    // new_session with parentSession (rpc.md §new_session).
    rpc.send(&json!({"id": "n1", "type": "new_session", "parentSession": "/tmp/parent.jsonl"}))
        .await;
    let response = rpc.next_response(Some("n1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["cancelled"], false);

    // The replacement rebinds: get_state reports the fresh session.
    rpc.send(&json!({"id": "n2", "type": "get_state"})).await;
    let response = rpc.next_response(Some("n2")).await;
    assert_eq!(response["data"]["messageCount"], 0);

    // switch_session to a file-backed session created beforehand.
    let other_dir = rpc.tmp.path().join("other-sessions");
    std::fs::create_dir_all(&other_dir).expect("other session dir");
    let mut other_manager =
        SessionManager::create(&rpc.cwd, Some(&other_dir), NewSessionOptions::default())
            .expect("create other session");
    let other_id = other_manager.get_session_id().to_owned();
    // SessionManager 延迟落盘：文件在首个 assistant 消息时才创建
    // (session-manager.ts `_persist`)，先补一条让 header 真正写入。
    other_manager
        .append_message(AgentMessage::Assistant(faux_assistant_message(
            "seed",
            FauxAssistantOptions::default(),
        )))
        .expect("append seed message");
    let other_path = other_manager
        .get_session_file()
        .expect("other session file")
        .to_path_buf();

    rpc.send(&json!({"id": "sw1", "type": "switch_session", "sessionPath": other_path.to_string_lossy()}))
        .await;
    let response = rpc.next_response(Some("sw1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["cancelled"], false);

    rpc.send(&json!({"id": "sw2", "type": "get_state"})).await;
    let response = rpc.next_response(Some("sw2")).await;
    assert_eq!(response["data"]["sessionId"], other_id);
    assert_eq!(
        response["data"]["sessionFile"].as_str().map(Path::new),
        Some(other_path.as_path())
    );

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// compact
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_command() {
    // 小会话默认可压缩性不足（keepRecentTokens=20000）；收紧阈值使切点
    // 恰好落在第二个 user 消息（turn 起点，单次摘要调用，避免 split-turn
    // 双调用）。
    let tmp = TempDir::new();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"compaction": {"keepRecentTokens": 5}}"#,
    )
    .expect("write settings.json");

    let mut rpc = boot(
        tmp,
        FauxProviderOptions {
            models: Some(default_models()),
            ..Default::default()
        },
        vec![
            assistant("first answer"),
            assistant("second answer"),
            assistant("Summary of the conversation so far."),
        ],
    )
    .await;

    rpc.send(&json!({"id": "p1", "type": "prompt", "message": "one"}))
        .await;
    rpc.next_response(Some("p1")).await;
    rpc.next_event("agent_settled").await;
    rpc.send(&json!({"id": "p2", "type": "prompt", "message": "two"}))
        .await;
    rpc.next_response(Some("p2")).await;
    rpc.next_event("agent_settled").await;

    rpc.send(&json!({"id": "cp1", "type": "compact", "customInstructions": "Focus on code"}))
        .await;
    // compaction_start / compaction_end events (rpc.md §compaction_start).
    let start = rpc.next_event("compaction_start").await;
    assert_eq!(start["reason"], "manual");
    let end = rpc.next_event("compaction_end").await;
    assert_eq!(end["reason"], "manual");
    assert_eq!(end["aborted"], false);

    let response = rpc.next_response(Some("cp1")).await;
    assert_eq!(response["success"], true, "compact response: {response}");
    let data = &response["data"];
    assert_eq!(data["summary"], "Summary of the conversation so far.");
    assert!(data["firstKeptEntryId"].as_str().is_some());
    assert!(data["tokensBefore"].as_u64().is_some());
    assert!(data["estimatedTokensAfter"].as_u64().is_some());

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// steer / follow_up / abort during streaming
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steer_follow_up_abort_during_streaming() {
    // 慢速流（tokens_per_second 限速）保证 prompt 在途时可交互。
    let long_text = "word ".repeat(400);
    let mut rpc = start_rpc_with(
        FauxProviderOptions {
            tokens_per_second: Some(60.0),
            ..Default::default()
        },
        vec![
            assistant(&long_text),
            assistant("after steer"),
            assistant("steer note answer"),
            assistant("later answer"),
        ],
    )
    .await;

    rpc.send(&json!({"id": "p1", "type": "prompt", "message": "start"}))
        .await;
    rpc.next_response(Some("p1")).await;
    rpc.next_event("agent_start").await;

    // prompt while streaming without streamingBehavior → rejection
    // (rpc.md §prompt "If the agent is streaming and no streamingBehavior
    // is specified, the command returns an error").
    rpc.send(&json!({"id": "p2", "type": "prompt", "message": "no behavior"}))
        .await;
    let response = rpc.next_response(Some("p2")).await;
    assert_eq!(response["success"], false);
    assert_eq!(response["command"], "prompt");
    assert!(response["error"]
        .as_str()
        .expect("error")
        .contains("streamingBehavior"));

    // prompt with streamingBehavior=steer queues instead (rpc.md §prompt).
    rpc.send(&json!({"id": "p3", "type": "prompt", "message": "steer via prompt", "streamingBehavior": "steer"}))
        .await;
    let (response, events) = rpc.next_response_with_events(Some("p3")).await;
    assert_eq!(response["success"], true);
    let queue = events
        .iter()
        .find(|e| e["type"] == "queue_update")
        .expect("queue_update event");
    assert_eq!(queue["steering"], json!(["steer via prompt"]));

    // steer command (rpc.md §steer).
    rpc.send(&json!({"id": "st1", "type": "steer", "message": "steer note"}))
        .await;
    assert_eq!(rpc.next_response(Some("st1")).await["success"], true);

    // follow_up command (rpc.md §follow_up).
    rpc.send(&json!({"id": "fu1", "type": "follow_up", "message": "later"}))
        .await;
    assert_eq!(rpc.next_response(Some("fu1")).await["success"], true);

    // abort stops the in-flight run (rpc.md §abort).
    rpc.send(&json!({"id": "ab1", "type": "abort"})).await;
    let (response, events) = rpc.next_response_with_events(Some("ab1")).await;
    assert_eq!(response["success"], true);
    assert!(
        events.iter().any(|e| e["type"] == "agent_settled"),
        "agent_settled missing: {events:?}"
    );

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// set_session_name / get_commands
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_name_and_get_commands() {
    let mut rpc = start_rpc(vec![]).await;

    // 空目录：无扩展命令/模板/技能（内置 TUI 命令不在其列，rpc.md §get_commands）。
    rpc.send(&json!({"id": "c1", "type": "get_commands"})).await;
    let response = rpc.next_response(Some("c1")).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["commands"], json!([]));

    // set_session_name + get_state.sessionName (rpc.md §set_session_name)。
    rpc.send(&json!({"id": "n1", "type": "set_session_name", "name": "my-session"}))
        .await;
    assert_eq!(rpc.next_response(Some("n1")).await["success"], true);
    rpc.send(&json!({"id": "n2", "type": "get_state"})).await;
    let response = rpc.next_response(Some("n2")).await;
    assert_eq!(response["data"]["sessionName"], "my-session");

    // Empty name → error (rpc-mode.ts:642-645).
    rpc.send(&json!({"id": "n3", "type": "set_session_name", "name": "   "}))
        .await;
    let response = rpc.next_response(Some("n3")).await;
    assert_eq!(response["success"], false);
    assert_eq!(response["error"], "Session name cannot be empty");

    assert_eq!(rpc.close_and_wait().await, 0);
}

/// get_commands 的 prompt 模板与 sourceInfo 重建（user scope，
/// prompt-templates.ts `getSourceInfo`）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_commands_with_prompt_template() {
    let tmp = TempDir::new();
    let prompts_dir = tmp.path().join("agent/prompts");
    std::fs::create_dir_all(&prompts_dir).expect("prompts dir");
    std::fs::write(
        prompts_dir.join("hello.md"),
        "---\ndescription: Greeting template\n---\n\nSay hello.\n",
    )
    .expect("write prompt template");

    let mut rpc = boot(
        tmp,
        FauxProviderOptions {
            models: Some(default_models()),
            ..Default::default()
        },
        vec![],
    )
    .await;
    rpc.send(&json!({"id": "c1", "type": "get_commands"})).await;
    let response = rpc.next_response(Some("c1")).await;
    let commands = response["data"]["commands"].as_array().expect("commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["name"], "hello");
    assert_eq!(commands[0]["description"], "Greeting template");
    assert_eq!(commands[0]["source"], "prompt");
    let source_info = &commands[0]["sourceInfo"];
    assert_eq!(source_info["scope"], "user");
    assert_eq!(source_info["origin"], "top-level");
    assert_eq!(source_info["source"], "local");
    assert!(source_info["path"]
        .as_str()
        .expect("path")
        .ends_with("hello.md"));

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// Protocol errors + framing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_errors_and_framing() {
    let mut rpc = start_rpc(vec![]).await;

    // 非法 JSON → command:"parse"（rpc.md §Error Handling）。
    rpc.send_raw("this is not json\n").await;
    let response = rpc.next_response(None).await;
    assert_eq!(response["command"], "parse");
    assert_eq!(response["success"], false);
    assert!(response["error"]
        .as_str()
        .expect("error")
        .starts_with("Failed to parse command: "));
    assert!(response.get("id").is_none());

    // 空行 / 纯空白行同样回 parse 错误（上游不过滤空行，JSON.parse("") 抛错）。
    rpc.send_raw("\n").await;
    let response = rpc.next_response(None).await;
    assert_eq!(response["command"], "parse");
    assert_eq!(response["success"], false);
    assert!(response["error"]
        .as_str()
        .expect("error")
        .starts_with("Failed to parse command: "));

    // 未知命令（rpc-mode.ts:695-698）。
    rpc.send(&json!({"id": "u1", "type": "frobnicate"})).await;
    let response = rpc.next_response(Some("u1")).await;
    assert_eq!(response["success"], false);
    assert_eq!(response["command"], "frobnicate");
    assert_eq!(response["error"], "Unknown command: frobnicate");

    // 非字符串 type：command 原值回显（上游 error(id, unknownCommand.type, ...)）。
    rpc.send_raw("{\"id\":\"u9\",\"type\":42}\n").await;
    let response = rpc.next_response(Some("u9")).await;
    assert_eq!(response["success"], false);
    assert_eq!(response["command"], 42);
    assert_eq!(response["error"], "Unknown command: 42");

    // 缺 type：command 键整键省略（JSON.stringify 丢弃 undefined）。
    rpc.send_raw("{\"id\":\"u10\"}\n").await;
    let response = rpc.next_response(Some("u10")).await;
    assert_eq!(response["success"], false);
    assert!(response.get("command").is_none());
    assert_eq!(response["error"], "Unknown command: undefined");

    // 已知命令但字段形状错误 → 边界拒绝（上游为 TypeError，同为 success:false）。
    rpc.send(&json!({"id": "u2", "type": "prompt"})).await;
    let response = rpc.next_response(Some("u2")).await;
    assert_eq!(response["success"], false);
    assert_eq!(response["command"], "prompt");

    // extension_ui_response 无匹配请求：静默忽略（后续命令不受影响）。
    rpc.send(&json!({"type": "extension_ui_response", "id": "nobody", "value": "x"}))
        .await;

    // U+2028/U+2029 在 payload 内不错拆 + CRLF 容忍（jsonl.ts 帧语义）。
    rpc.send_raw(
        "{\"id\":\"f1\",\"type\":\"set_session_name\",\"name\":\"a\u{2028}b\u{2029}c\"}\r\n",
    )
    .await;
    let response = rpc.next_response(Some("f1")).await;
    assert_eq!(response["success"], true);
    rpc.send(&json!({"id": "f2", "type": "get_state"})).await;
    let response = rpc.next_response(Some("f2")).await;
    assert_eq!(response["data"]["sessionName"], "a\u{2028}b\u{2029}c");

    // EOF 前无换行的尾行仍被处理（jsonl.ts:43-49），随后 EOF 正常关闭。
    rpc.send_raw("{\"id\":\"f3\",\"type\":\"get_available_thinking_levels\"}")
        .await;
    assert_eq!(rpc.close_and_wait().await, 0);
    let response = rpc.next_response(Some("f3")).await;
    assert_eq!(response["success"], true);
    assert_eq!(
        response["data"]["levels"],
        json!(["off", "minimal", "low", "medium", "high"])
    );
}

// ---------------------------------------------------------------------------
// export_html 占位（T14）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_html_placeholder() {
    let mut rpc = start_rpc(vec![]).await;
    rpc.send(&json!({"id": "x1", "type": "export_html"})).await;
    let response = rpc.next_response(Some("x1")).await;
    assert_eq!(response["success"], false);
    assert!(response["error"]
        .as_str()
        .expect("error")
        .contains("not available yet"));

    assert_eq!(rpc.close_and_wait().await, 0);
}

// ---------------------------------------------------------------------------
// 信号退出码（print-mode.ts / rpc-mode.ts：SIGTERM=143 / SIGHUP=129）
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn signal_exit_code(signal: libc::c_int, expected_code: i32) {
    let tmp = TempDir::new();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("models.json"),
        r#"{"providers":{"faux":{"name":"Faux","baseUrl":"http://127.0.0.1:9/unreachable","api":"anthropic-messages","apiKey":"FAUX_E2E_KEY","models":[{"id":"faux-1","name":"Faux One","reasoning":true,"input":["text"],"cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":0.2},"contextWindow":200000,"maxTokens":8192}]}}}"#,
    )
    .expect("write models.json");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pir-rpc"))
        .args([
            "--no-session",
            "--provider",
            "faux",
            "--model",
            "faux/faux-1",
        ])
        .env("PIR_CODING_AGENT_DIR", &agent_dir)
        .env("FAUX_E2E_KEY", "dummy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn pir-rpc");

    // 等启动管线完成（rpc 循环就位），再发信号。
    std::thread::sleep(std::time::Duration::from_millis(800));
    unsafe {
        libc::kill(child.id() as libc::pid_t, signal);
    }
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(expected_code));
}

#[cfg(unix)]
#[test]
fn rpc_mode_sigterm_exits_143() {
    signal_exit_code(libc::SIGTERM, 143);
}

#[cfg(unix)]
#[test]
fn rpc_mode_sighup_exits_129() {
    signal_exit_code(libc::SIGHUP, 129);
}

#[test]
fn pir_rpc_bin_end_to_end() {
    let tmp = TempDir::new();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("models.json"),
        r#"{
  "providers": {
    "faux": {
      "name": "Faux",
      "baseUrl": "http://127.0.0.1:9/unreachable",
      "api": "anthropic-messages",
      "apiKey": "FAUX_E2E_KEY",
      "models": [
        {
          "id": "faux-1",
          "name": "Faux One",
          "reasoning": true,
          "input": ["text"],
          "cost": {"input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 0.2},
          "contextWindow": 200000,
          "maxTokens": 8192
        }
      ]
    }
  }
}"#,
    )
    .expect("write models.json");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir-rpc"))
        .args([
            "--no-session",
            "--provider",
            "faux",
            "--model",
            "faux/faux-1",
        ])
        .env("PIR_CODING_AGENT_DIR", &agent_dir)
        .env("FAUX_E2E_KEY", "dummy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"{\"id\":\"e1\",\"type\":\"get_state\"}\n")?;
            child.wait_with_output()
        })
        .expect("run pir-rpc");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let response: Value =
        serde_json::from_str(stdout.lines().next().expect("one line")).expect("json response");
    assert_eq!(response["id"], "e1");
    assert_eq!(response["command"], "get_state");
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["model"]["id"], "faux-1");
}
