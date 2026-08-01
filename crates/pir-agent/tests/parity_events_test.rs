//! G3 对拍：`fixtures/generated/<scenario>/events.jsonl`（上游 `createAgentSession`
//! + faux provider 实录）vs `pir-agent` 的 `Agent` 在相同脚本下产出的事件序列。
//!
//! 对拍粒度（见 fixtures/README.md §2 与测试内注释）：
//! - `message_update` 事件整类排除：上游 faux 用 `Math.random` 切 delta，delta
//!   边界与数量不入契约；pir 侧 faux 为确定性切块。
//! - `queue_update` / `agent_settled` / `agent_end.willRetry` 排除：这些是
//!   coding-agent `AgentSession` 层（T16）事件，`Agent` 层不产生。
//! - `usage` 键排除：usage 由 faux 按完整 session context（系统提示 + 工具
//!   清单，session/harness 层 T07/T16 的产物）估算，Agent 层无法 1:1 复现。
//! - `details` 键排除：fixture 的 read/bash 是上游真实工具（T13），其结果
//!   details 形状不属于 Agent 层契约；测试工具只复现 content。
//! - 其余内容（事件类型序、消息/toolResult 载荷、stopReason、工具调用参数、
//!   完成序/源序语义）在 `pir_test_support` 归一化（timestamp/id 剥离）后做
//!   行序敏感的 JSONL diff。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use pir_agent::agent::AgentListener;
use pir_agent::messages::AgentMessage;
use pir_agent::types::{AgentEvent, AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pir_agent::{Agent, AgentOptions, InitialAgentState};
use pir_ai::types::{StopReason, TextContent, ToolResultContent};
use pir_test_support::diff::diff_jsonl;
use pir_test_support::faux::{
    faux_assistant_message, faux_text, faux_tool_call, FauxAssistantOptions, FauxProvider,
    FauxProviderOptions,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated")
}

// ---------------------------------------------------------------------------
// Fixture / actual-line preparation
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

/// Filter session-layer event types and strip non-Agent-layer keys, then
/// re-render as JSONL (one compact JSON object per line).
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
    // Leak the unsubscribe closure: the listener must outlive the whole test.
    std::mem::forget(_unsubscribe);
    events
}

/// Poll the collected events until one at index >= `from` satisfies `pred`.
async fn wait_for_event(
    events: &Arc<Mutex<Vec<AgentEvent>>>,
    from: usize,
    pred: impl Fn(&AgentEvent) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            {
                let events = events.lock().unwrap();
                if events.iter().skip(from).any(&pred) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("timed out waiting for expected event");
}

fn user_blocks_message(text: &str) -> AgentMessage {
    AgentMessage::User(pir_ai::types::UserMessage {
        role: pir_ai::types::UserRole::User,
        content: pir_ai::types::UserContent::Blocks(vec![pir_ai::types::UserContentBlock::Text(
            TextContent {
                text: text.to_owned(),
                text_signature: None,
            },
        )]),
        timestamp: 1,
    })
}

// ---------------------------------------------------------------------------
// Scenario tools (fixture `tool-calls`): content-compatible read/bash stand-ins
// ---------------------------------------------------------------------------

type FixtureExecuteFn = Arc<
    dyn Fn(
            Option<AgentToolUpdateCallback>,
        ) -> BoxFuture<'static, Result<AgentToolResult, pir_agent::AgentError>>
        + Send
        + Sync,
>;

struct FixtureTool {
    name: String,
    parameters: Value,
    execute_fn: FixtureExecuteFn,
}

#[async_trait]
impl AgentTool for FixtureTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn label(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "fixture tool"
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _signal: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, pir_agent::AgentError> {
        (self.execute_fn)(on_update).await
    }
}

fn text_tool_result(text: &str) -> Result<AgentToolResult, pir_agent::AgentError> {
    Ok(AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        // `null` details: omitted from the toolResult message (upstream
        // `undefined`); stripped from tool_execution_* events anyway.
        details: Value::Null,
        ..Default::default()
    })
}

/// `read`: slower than `bash` so completion order (bash, read) matches the
/// fixture while source order (read, bash) is preserved in the artifacts.
fn fixture_read_tool() -> FixtureTool {
    FixtureTool {
        name: "read".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        }),
        execute_fn: Arc::new(|_on_update| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                text_tool_result("fixture note content\n")
            })
        }),
    }
}

/// `bash`: emits the two partial-result updates the upstream bash tool
/// produces (initial empty content, then the captured output).
fn fixture_bash_tool() -> FixtureTool {
    FixtureTool {
        name: "bash".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"],
        }),
        execute_fn: Arc::new(|on_update| {
            Box::pin(async move {
                if let Some(on_update) = &on_update {
                    on_update(AgentToolResult {
                        content: Vec::new(),
                        details: Value::Null,
                        ..Default::default()
                    });
                    on_update(AgentToolResult {
                        content: vec![ToolResultContent::Text(TextContent {
                            text: "fixture-bash-output\n".to_owned(),
                            text_signature: None,
                        })],
                        details: json!({}),
                        ..Default::default()
                    });
                }
                text_tool_result("fixture-bash-output\n")
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parity_single_turn() {
    async fn scenario() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![faux_assistant_message(
            "Hello from the faux provider!",
            FauxAssistantOptions::default(),
        )
        .into()]);
        let agent = agent_for(&provider, Vec::new());
        let events = subscribe(&agent);

        agent.prompt("Say hello.").await.expect("prompt resolves");
        compare_with_fixture("single-turn", &events.lock().unwrap());
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test]
async fn parity_tool_calls() {
    async fn scenario() {
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
        let tools: Vec<Arc<dyn AgentTool>> =
            vec![Arc::new(fixture_read_tool()), Arc::new(fixture_bash_tool())];
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

#[tokio::test]
async fn parity_abort() {
    async fn scenario() {
        let provider = FauxProvider::new(FauxProviderOptions {
            tokens_per_second: Some(50.0),
            ..Default::default()
        });
        provider.set_responses(vec![faux_assistant_message(
            "A long answer that the user will abort before it finishes streaming. ".repeat(8),
            FauxAssistantOptions::default(),
        )
        .into()]);
        let agent = Arc::new(agent_for(&provider, Vec::new()));
        let events = subscribe(&agent);

        let prompt_agent = agent.clone();
        let prompt_handle =
            tokio::spawn(async move { prompt_agent.prompt("Give me a long answer.").await });
        wait_for_event(&events, 0, |e| {
            matches!(e, AgentEvent::MessageUpdate { .. })
        })
        .await;
        agent.abort();
        prompt_handle
            .await
            .expect("prompt task")
            .expect("prompt resolves");

        compare_with_fixture("abort", &events.lock().unwrap());
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test]
async fn parity_length_truncation() {
    async fn scenario() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![faux_assistant_message(
            "Truncated answer that hit the max token limit",
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Length),
                ..Default::default()
            },
        )
        .into()]);
        let agent = agent_for(&provider, Vec::new());
        let events = subscribe(&agent);

        agent
            .prompt("Answer until you run out of tokens.")
            .await
            .expect("prompt resolves");
        compare_with_fixture("length-truncation", &events.lock().unwrap());
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

/// steering-followup：fixture 含 `queue_update`（AgentSession 层）事件，
/// 过滤后 Agent 层事件序列与载荷仍可做到归一化内容级一致。
#[tokio::test]
async fn parity_steering_followup() {
    async fn scenario() {
        let provider = FauxProvider::new(FauxProviderOptions {
            tokens_per_second: Some(50.0),
            ..Default::default()
        });
        provider.set_responses(vec![
            faux_assistant_message(
                "This is a long first answer that keeps streaming so a steering message can interrupt it mid-turn. "
                    .repeat(8),
                FauxAssistantOptions::default(),
            )
            .into(),
            faux_assistant_message(
                "Answer after the steering interruption.",
                FauxAssistantOptions::default(),
            )
            .into(),
            faux_assistant_message(
                "Second answer, also long enough that a follow-up message can be queued while it streams. "
                    .repeat(8),
                FauxAssistantOptions::default(),
            )
            .into(),
            faux_assistant_message(
                "Final answer after steering and follow-up.",
                FauxAssistantOptions::default(),
            )
            .into(),
        ]);
        let agent = Arc::new(agent_for(&provider, Vec::new()));
        let events = subscribe(&agent);

        // Run 1: steer mid-stream.
        let prompt_agent = agent.clone();
        let first =
            tokio::spawn(async move { prompt_agent.prompt("Start the first answer.").await });
        wait_for_event(&events, 0, |e| {
            matches!(e, AgentEvent::MessageUpdate { .. })
        })
        .await;
        agent.steer(user_blocks_message("Change of plans: answer briefly."));
        first.await.expect("prompt task").expect("prompt resolves");

        // Run 2: follow-up mid-stream.
        let marker = events.lock().unwrap().len();
        let prompt_agent = agent.clone();
        let second =
            tokio::spawn(async move { prompt_agent.prompt("Now the second answer.").await });
        wait_for_event(&events, marker, |e| {
            matches!(e, AgentEvent::MessageUpdate { .. })
        })
        .await;
        agent.follow_up(user_blocks_message("And one more thing."));
        second.await.expect("prompt task").expect("prompt resolves");

        compare_with_fixture("steering-followup", &events.lock().unwrap());
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}
