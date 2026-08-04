//! T08 对拍：`fixtures/generated/compaction-{threshold,overflow}/`（上游
//! `createAgentSession` + faux 实录）vs `pir::core::compaction_runner::
//! CompactionRunner` 驱动的相同脚本。
//!
//! 测试内复刻 agent-session 的最小接线（`_handleAgentEvent` 的持久化 +
//! `_runAgentPrompt` 的 post-run 检查循环 + prompt 提交前检查，
//! agent-session.ts:595-665/1061-1103/1197-1202）——这部分归 T10 的
//! AgentSession 吸纳，此处只为对拍而复刻。
//!
//! 归一化约定（在 `pir_test_support` Normalizer 之上追加）：
//! - `usage` 整键剥离：faux usage 由完整 context（含系统提示）估算且有
//!   prompt-cache 双计；上游 fixture 的系统提示是 coding-agent 构建器产物
//!   （T12 才移植），pir 侧用定长填充系统提示，数值不可比。
//! - `tokensBefore` / `estimatedTokensAfter` 同理（由 usage 锚点推出）。
//!   触发*决策*（何时压缩、切点、摘要内容、firstKeptEntryId、willRetry、
//!   事件类型序）仍在契约内，逐值比对。
//! - session 头 `cwd` 替换为占位符（临时目录路径）。
//! - 事件比对只取 compaction 事件子集（compaction_start/compaction_end/
//!   summarization_retry_*）；Agent 层事件不属于 runner 契约。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::core::compaction_runner::{CompactionEvent, CompactionRunner};
use pir::core::session_manager::{NewSessionOptions, SessionManager};
use pir_agent::compaction::CompactionSettings;
use pir_agent::messages::AgentMessage;
use pir_agent::types::{AgentEvent, ThinkingLevel};
use pir_agent::{Agent, AgentOptions, InitialAgentState};
use pir_ai::types::{AssistantMessage, StopReason};
use pir_test_support::diff::diff_jsonl;
use pir_test_support::faux::{
    faux_assistant_message, FauxAssistantOptions, FauxModelDefinition, FauxProvider,
    FauxProviderOptions, FauxResponseStep,
};
use serde_json::Value;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-parity-compaction-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        TestDir(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 定长填充系统提示（~1000 tokens）：触发阈值需要与上游相当的整体用量，
/// 但具体文本不属于契约（usage 数值剥离）。
fn filler_system_prompt() -> String {
    "fixture system prompt filler. ".repeat(140)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 脚本响应：调用时刻的新鲜时间戳（上游工厂响应语义）。固定/0 时间戳会被
/// stale-usage 守卫拦下（agent-session.ts:1974），压缩链将静默断掉。
///
/// 防并列：毫秒级时钟下，压缩条目写入（真实时钟）与下一条脚本响应若落在
/// 同一毫秒，`assistantMessage.timestamp <= compactionTs` 守卫会误杀后续
/// 检查（并行测试放大此窗口）。工厂先让出 5ms 再取严格递增时间戳。
fn scripted(content: impl Into<String>) -> FauxResponseStep {
    static LAST_TS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let content = content.into();
    FauxResponseStep::Factory(Box::new(move |_context, _options, _state, _model| {
        std::thread::sleep(std::time::Duration::from_millis(5));
        let now = now_ms();
        // CAS loop: strictly increasing timestamps (`fetch_update` reports
        // the *previous* value, which would shift the sequence by one).
        let timestamp = loop {
            let last = LAST_TS.load(std::sync::atomic::Ordering::SeqCst);
            let next = last.max(now - 1) + 1;
            if LAST_TS
                .compare_exchange(
                    last,
                    next,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                break next;
            }
        };
        let mut message = faux_assistant_message(content.clone(), FauxAssistantOptions::default());
        message.timestamp = timestamp;
        message
    }))
}

fn scripted_overflow_error() -> FauxResponseStep {
    FauxResponseStep::Factory(Box::new(move |_context, _options, _state, _model| {
        let mut message = faux_assistant_message(
            "",
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("prompt is too long: 200000 tokens > 16384 maximum".to_owned()),
                ..Default::default()
            },
        );
        message.timestamp = now_ms();
        message
    }))
}

// ---------------------------------------------------------------------------
// Harness：agent-session 最小接线的复刻
// ---------------------------------------------------------------------------

struct Harness {
    agent: Arc<Agent>,
    runner: CompactionRunner,
    agent_events: Arc<Mutex<Vec<AgentEvent>>>,
    compaction_events: Arc<Mutex<Vec<CompactionEvent>>>,
    /// `_lastAssistantMessage`（agent-session.ts:647）：post-run 检查的消费源。
    last_assistant: Option<AssistantMessage>,
}

impl Harness {
    fn new(
        dir: &Path,
        context_window: u32,
        settings: CompactionSettings,
        responses: Vec<FauxResponseStep>,
    ) -> Self {
        let provider = FauxProvider::new(FauxProviderOptions {
            models: Some(vec![FauxModelDefinition {
                id: "faux-1".to_owned(),
                name: None,
                reasoning: None,
                input: None,
                cost: None,
                context_window: Some(context_window),
                max_tokens: Some(65536),
            }]),
            ..Default::default()
        });
        provider.set_responses(responses);
        let model = provider.get_model(None).expect("faux-1 registered");

        let mut options = AgentOptions::new(provider.stream_fn());
        options.initial_state = InitialAgentState {
            system_prompt: Some(filler_system_prompt()),
            model: Some(model.clone()),
            thinking_level: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let mut agent = Agent::new(options);
        // agent-session 把 session id 透传给 provider（prompt-cache 模拟的键）。
        agent.session_id = Some("parity-compaction-session".to_owned());
        let agent = Arc::new(agent);

        let agent_events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let listener_events = agent_events.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _signal| {
            let listener_events = listener_events.clone();
            Box::pin(async move {
                listener_events.lock().expect("events").push(event);
            })
        }));
        std::mem::forget(unsubscribe);

        let session_dir = dir.join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let session = SessionManager::create(dir, Some(&session_dir), NewSessionOptions::default())
            .expect("create session");
        let compaction_events: Arc<Mutex<Vec<CompactionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = compaction_events.clone();
        let runner = CompactionRunner::new(
            agent.clone(),
            Arc::new(Mutex::new(session)),
            Some(model),
            settings,
            None,
            provider.stream_fn(),
            ThinkingLevel::Off,
            Arc::new(move |event| sink_events.lock().expect("events").push(event)),
        );

        let harness = Self {
            agent,
            runner,
            agent_events,
            compaction_events,
            last_assistant: None,
        };
        // createAgentSession 的初始条目序（fixture 第 2/3 行）。
        harness
            .runner
            .session_mut()
            .append_model_change("faux", "faux-1")
            .expect("append model_change");
        harness
            .runner
            .session_mut()
            .append_thinking_level_change("off")
            .expect("append thinking_level_change");
        harness
    }

    /// `_handleAgentEvent` 的持久化部分（agent-session.ts:624-652）：
    /// message_end 的 user/assistant/toolResult 落盘；user 回合开始与非错误
    /// assistant 回复重置 overflow 恢复预算；跟踪 `_lastAssistantMessage`。
    fn drain_agent_events(&mut self) {
        let events: Vec<AgentEvent> =
            std::mem::take(&mut *self.agent_events.lock().expect("events"));
        for event in events {
            match event {
                AgentEvent::MessageStart { message, .. } => {
                    if matches!(message, AgentMessage::User(_)) {
                        self.runner.reset_overflow_recovery();
                    }
                }
                AgentEvent::MessageEnd { message, .. } => match &message {
                    AgentMessage::User(_) | AgentMessage::ToolResult(_) => {
                        self.runner
                            .session_mut()
                            .append_message(message.clone())
                            .expect("append message");
                    }
                    AgentMessage::Assistant(assistant) => {
                        self.runner
                            .session_mut()
                            .append_message(message.clone())
                            .expect("append message");
                        if assistant.stop_reason != StopReason::Error {
                            self.runner.reset_overflow_recovery();
                        }
                        self.last_assistant = Some(assistant.clone());
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    /// `_runAgentPrompt`（agent-session.ts:1061-1073）+ prompt 提交前检查
    /// （:1197-1202）。pre-prompt 检查不 continue。
    async fn prompt(&mut self, text: &str) {
        if let Some(assistant) = self.find_last_assistant_in_state() {
            self.runner.check_compaction(&assistant, false).await;
        }

        self.agent.prompt(text).await.expect("prompt resolves");
        self.drain_agent_events();

        // `_handlePostAgentRun` 循环：check → continue → check → …
        while let Some(assistant) = self.last_assistant.take() {
            if self.runner.check_compaction(&assistant, true).await {
                self.agent.continue_run().await.expect("continue resolves");
                self.drain_agent_events();
            } else {
                break;
            }
        }
    }

    /// `_findLastAssistantMessage`（agent-session.ts:684-693）。
    fn find_last_assistant_in_state(&self) -> Option<AssistantMessage> {
        self.agent
            .state()
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                AgentMessage::Assistant(a) => Some(a.clone()),
                _ => None,
            })
    }
}

// ---------------------------------------------------------------------------
// Comparison preparation
// ---------------------------------------------------------------------------

const STRIPPED_KEYS: &[&str] = &["usage", "tokensBefore", "estimatedTokensAfter"];
const COMPACTION_EVENT_TYPES: &[&str] = &[
    "compaction_start",
    "compaction_end",
    "summarization_retry_scheduled",
    "summarization_retry_attempt_start",
    "summarization_retry_finished",
];

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

/// session.jsonl 逐行准备：剥离 usage 系数值、占位 cwd，重新渲染为 JSONL。
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

/// events 逐行准备：只保留 compaction 事件子集，剥离 usage 系数值。
fn prepare_event_lines(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_str(line).expect("event line is JSON");
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if !COMPACTION_EVENT_TYPES.contains(&event_type) {
            continue;
        }
        strip_keys(&mut value);
        out.push_str(&serde_json::to_string(&value).expect("render"));
        out.push('\n');
    }
    out
}

fn compare(harness: &Harness, scenario: &str) {
    // Session file parity.
    let session_file = harness
        .runner
        .session()
        .get_session_file()
        .map(|path| path.to_path_buf())
        .expect("file-backed session");
    let actual_session = std::fs::read_to_string(session_file).expect("read actual session");
    let expected_session =
        std::fs::read_to_string(fixtures_dir().join(scenario).join("session.jsonl"))
            .expect("read fixture session");
    diff_jsonl(
        &prepare_session_lines(&expected_session),
        &prepare_session_lines(&actual_session),
    )
    .unwrap_or_else(|f| panic!("{scenario}: session parity diff:\n{f}"));

    // Compaction event parity.
    let mut actual_events = String::new();
    for event in harness.compaction_events.lock().expect("events").iter() {
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
    .unwrap_or_else(|f| panic!("{scenario}: compaction event parity diff:\n{f}"));
}

// ---------------------------------------------------------------------------
// Scenarios (scripts mirror fixtures/generate-fixtures.mjs)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parity_compaction_threshold() {
    async fn scenario() {
        let dir = TestDir::new("threshold");
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 4096,
            keep_recent_tokens: 512,
        };
        let responses = vec![
            scripted(format!("ALPHA {}", "alpha evidence block. ".repeat(560))),
            scripted("Turn prefix summary: the user asked about the alpha topic."),
            scripted(format!("BETA {}", "beta evidence block. ".repeat(560))),
            scripted("Updated history summary: alpha and beta evidence discussed."),
            scripted("Turn prefix summary: the user then asked about beta."),
            scripted("A short final answer."),
        ];
        let mut harness = Harness::new(&dir.0, 8192, settings, responses);
        harness
            .prompt("First question about the alpha topic.")
            .await;
        harness
            .prompt("Second question about the beta topic.")
            .await;
        harness.prompt("A small follow-up question.").await;
        compare(&harness, "compaction-threshold");
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}

#[tokio::test]
async fn parity_compaction_overflow() {
    async fn scenario() {
        let dir = TestDir::new("overflow");
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 8192,
            keep_recent_tokens: 256,
        };
        let responses = vec![
            scripted(format!("FIRST {}", "first answer block. ".repeat(80))),
            scripted(format!("SECOND {}", "second answer block. ".repeat(80))),
            scripted_overflow_error(),
            scripted("History summary: two answers were given before the overflow."),
            scripted("Turn prefix summary: the user asked the overflowing question."),
            scripted("Recovered answer after compaction and retry."),
        ];
        let mut harness = Harness::new(&dir.0, 16384, settings, responses);
        harness.prompt("Question one.").await;
        harness.prompt("Question two.").await;
        harness
            .prompt("The question that overflows the context window.")
            .await;
        compare(&harness, "compaction-overflow");
    }
    tokio::time::timeout(TEST_TIMEOUT, scenario())
        .await
        .expect("must not hang");
}
