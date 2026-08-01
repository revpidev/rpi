//! G3 对拍（T06）：`fixtures/generated/tool-calls/events.jsonl`（上游真实工具
//! 实录）vs `pir::tools` 的真实 read/bash 工具在相同 faux 脚本下产出的事件序列。
//!
//! 对拍粒度与 `pir-agent/tests/parity_events_test.rs` 一致（`message_update`
//! 整类排除、`usage`/`willRetry`/`details` 键排除、归一化后行序敏感 diff）。
//!
//! 与 T05 对拍的两点差异：
//! - 工具换成真实实现：`read` 读真实临时目录中的 `note.txt`（内容与上游
//!   fixture  stub 的输出一致），`bash` 真实 spawn `echo fixture-bash-output`。
//! - `read` 包了一层延迟 30ms 的 `ReadOperations`：fixture 中并行工具的完成序
//!   是 [bash, read]（上游实录时机如此），而完成序本身是计时相关的。延迟只
//!   作用在 IO 层，工具的路径解析/读取/截断逻辑全部为真实路径。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pir_agent::agent::AgentListener;
use pir_agent::types::{AgentEvent, AgentTool};
use pir_agent::{Agent, AgentOptions, InitialAgentState};
use pir_ai::types::StopReason;
use pir_test_support::diff::diff_jsonl;
use pir_test_support::faux::{
    faux_assistant_message, faux_text, faux_tool_call, FauxAssistantOptions, FauxProvider,
    FauxProviderOptions,
};
use serde_json::{json, Value};

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated")
}

// ---------------------------------------------------------------------------
// Fixture / actual-line preparation (same granularity as pir-agent parity test)
// ---------------------------------------------------------------------------

const DROPPED_EVENT_TYPES: &[&str] = &["message_update", "queue_update", "agent_settled"];
const STRIPPED_KEYS: &[&str] = &["usage", "willRetry", "details"];

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

fn prepare_lines(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_str(line).expect("fixture/actual line is JSON");
        if let Some(event_type) = value.get("type").and_then(Value::as_str) {
            if DROPPED_EVENT_TYPES.contains(&event_type) {
                continue;
            }
        }
        strip_keys(&mut value);
        out.push_str(&serde_json::to_string(&value).expect("render"));
        out.push('\n');
    }
    out
}

fn compare_with_fixture(scenario: &str, events: &[AgentEvent]) {
    let fixture_path = fixtures_dir().join(scenario).join("events.jsonl");
    let expected_raw = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", fixture_path.display()));
    let expected = prepare_lines(&expected_raw);

    let mut actual_raw = String::new();
    for event in events {
        actual_raw.push_str(&serde_json::to_string(event).expect("serialize AgentEvent"));
        actual_raw.push('\n');
    }
    let actual = prepare_lines(&actual_raw);

    diff_jsonl(&expected, &actual)
        .unwrap_or_else(|f| panic!("parity diff for scenario `{scenario}`:\n{f}"));
}

// ---------------------------------------------------------------------------
// Agent driver helpers
// ---------------------------------------------------------------------------

fn collecting_listener() -> (AgentListener, Arc<Mutex<Vec<AgentEvent>>>) {
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let listener_events = events.clone();
    let listener: AgentListener = Arc::new(move |event, _signal| {
        let listener_events = listener_events.clone();
        Box::pin(async move {
            listener_events.lock().unwrap().push(event);
        })
    });
    (listener, events)
}

fn agent_for(provider: &Arc<FauxProvider>, tools: Vec<Arc<dyn AgentTool>>) -> Agent {
    let mut options = AgentOptions::new(provider.stream_fn());
    options.initial_state = InitialAgentState {
        model: provider.get_model(None),
        tools: Some(tools),
        ..Default::default()
    };
    Agent::new(options)
}

fn subscribe(agent: &Agent) -> Arc<Mutex<Vec<AgentEvent>>> {
    let (listener, events) = collecting_listener();
    let _unsubscribe = agent.subscribe(listener);
    std::mem::forget(_unsubscribe);
    events
}

// ---------------------------------------------------------------------------
// Delayed read operations: pins the parallel completion order to the fixture's
// ---------------------------------------------------------------------------

struct DelayedReadOperations {
    inner: Arc<dyn pir::tools::read::ReadOperations>,
    delay: Duration,
}

#[async_trait]
impl pir::tools::read::ReadOperations for DelayedReadOperations {
    async fn read_file(&self, absolute_path: &Path) -> std::io::Result<Vec<u8>> {
        tokio::time::sleep(self.delay).await;
        self.inner.read_file(absolute_path).await
    }

    async fn access(&self, absolute_path: &Path) -> std::io::Result<()> {
        self.inner.access(absolute_path).await
    }

    async fn detect_image_mime_type(
        &self,
        absolute_path: &Path,
    ) -> std::io::Result<Option<String>> {
        self.inner.detect_image_mime_type(absolute_path).await
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-parity-tools-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create parity test dir");
        TestDir(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Scenario: real read + bash against the `tool-calls` fixture
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parity_tool_calls_real_tools() {
    async fn scenario() {
        let dir = TestDir::new();
        std::fs::write(dir.0.join("note.txt"), "fixture note content\n").expect("write note.txt");

        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![
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
        ]);

        let ctx = pir::tools::ToolContext {
            cwd: dir.0.clone(),
            session_env: None,
        };
        let delayed_read = DelayedReadOperations {
            inner: Arc::new(pir::tools::read::LocalReadOperations),
            delay: Duration::from_millis(30),
        };
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            pir::tools::read::create_read_tool(
                &ctx,
                pir::tools::read::ReadToolOptions {
                    operations: Some(Arc::new(delayed_read)),
                    ..Default::default()
                },
            ),
            pir::tools::bash::create_bash_tool(&ctx, pir::tools::bash::BashToolOptions::default()),
        ];

        let agent = agent_for(&provider, tools);
        let events = subscribe(&agent);

        agent
            .prompt("Read note.txt and run the echo command.")
            .await
            .expect("prompt resolves");
        compare_with_fixture("tool-calls", &events.lock().unwrap());
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}
