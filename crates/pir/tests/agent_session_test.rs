//! `AgentSession` 单元测试：prompt 生命周期事件序、queue_update、auto-retry、
//! bash 结果在流式期间的暂存与下个 prompt 前的 flush。
//!
//! 与 `rpc_mode_test.rs` 的全栈契约测试互补：这里直接驱动 `AgentSession`
//! （经 `create_agent_session` 组装 + FauxProvider 脚本化响应），不走 RPC 线协议。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use pir::core::agent_session::{AgentSession, AgentSessionEvent, PromptOptions};
use pir::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use pir::core::model_runtime::{CreateModelRuntimeOptions, ModelsPathInput};
use pir::core::session_manager::{NewSessionOptions, SessionManager};
use pir_agent::messages::AgentMessage;
use pir_test_support::faux::{
    faux_assistant_message, FauxAiProvider, FauxAssistantOptions, FauxModelDefinition,
    FauxProvider, FauxProviderOptions, FauxResponseStep,
};

// ---------------------------------------------------------------------------
// Fixture
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
        let dir = std::env::temp_dir().join(format!(
            "pir-agent-session-test-{}-{nanos}-{id}",
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

fn assistant(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

fn error_step(error_message: &str) -> FauxResponseStep {
    faux_assistant_message(
        "",
        FauxAssistantOptions {
            stop_reason: Some(pir_ai::types::StopReason::Error),
            error_message: Some(error_message.to_owned()),
            ..Default::default()
        },
    )
    .into()
}

struct SessionFixture {
    session: AgentSession,
    provider: Arc<FauxProvider>,
    events: Arc<Mutex<Vec<AgentSessionEvent>>>,
    _tmp: TempDir,
}

impl SessionFixture {
    fn event_types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|event| {
                serde_json::to_value(event)
                    .expect("event serializes")
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            })
            .collect()
    }

    fn events_of_type(&self, event_type: &str) -> Vec<AgentSessionEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|event| {
                serde_json::to_value(event)
                    .expect("event serializes")
                    .get("type")
                    .and_then(Value::as_str)
                    == Some(event_type)
            })
            .cloned()
            .collect()
    }
}

async fn session_fixture(
    responses: Vec<FauxResponseStep>,
    provider_options: FauxProviderOptions,
    settings_json: Option<&str>,
) -> SessionFixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    if let Some(settings) = settings_json {
        std::fs::write(agent_dir.join("settings.json"), settings).expect("write settings");
    }

    let mut options = provider_options;
    if options.models.is_none() {
        options.models = Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: Some("Faux One".to_owned()),
            reasoning: Some(true),
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]);
    }
    let provider = FauxProvider::new(options);
    provider.set_responses(responses);
    let model = provider.get_model(None).expect("faux model");

    let model_runtime = pir::core::model_runtime::ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: None,
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
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

    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory(Some(&cwd), NewSessionOptions::default())
            .expect("in-memory session"),
    ));
    let created = pir::sdk::create_agent_session(pir::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
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
    // 测试期间订阅一直存活：unsubscribe 句柄泄漏到 fixture 生命周期结束。
    std::mem::forget(_unsubscribe);

    SessionFixture {
        session: created.session,
        provider,
        events,
        _tmp: tmp,
    }
}

// ---------------------------------------------------------------------------
// prompt 生命周期：事件序 + 消息 + session 持久化
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_lifecycle_events_and_persistence() {
    let fixture = session_fixture(
        vec![assistant("hi there")],
        FauxProviderOptions::default(),
        None,
    )
    .await;

    fixture
        .session
        .prompt("hello", PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    // 事件序：agent_start 最先、agent_settled 最后，中间含 turn/message 面。
    let types = fixture.event_types();
    assert_eq!(types.first().map(String::as_str), Some("agent_start"));
    assert_eq!(types.last().map(String::as_str), Some("agent_settled"));
    for expected in [
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(
            types.iter().any(|t| t == expected),
            "missing {expected} in {types:?}"
        );
    }

    // agent_end 带 willRetry:false（rpc.md §agent_end）。
    let agent_end = fixture.events_of_type("agent_end");
    assert_eq!(agent_end.len(), 1);
    let value = serde_json::to_value(&agent_end[0]).expect("serialize");
    assert_eq!(value["willRetry"], false);
    assert_eq!(value["messages"].as_array().expect("messages").len(), 2);

    // 消息状态：user + assistant。
    let messages = fixture.session.messages();
    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], AgentMessage::User(_)));
    assert!(matches!(messages[1], AgentMessage::Assistant(_)));
    assert_eq!(
        fixture.session.get_last_assistant_text().as_deref(),
        Some("hi there")
    );

    // session 持久化（message_end 先写 session 再转发听众，agent-session.ts:752）。
    let entries = {
        let manager = fixture.session.session_manager();
        let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
        manager.get_entries()
    };
    let message_entries: Vec<_> = entries
        .iter()
        .filter(|entry| entry.type_tag() == "message")
        .collect();
    assert_eq!(message_entries.len(), 2);
    assert_eq!(fixture.provider.call_count(), 1);
}

// ---------------------------------------------------------------------------
// queue_update：steer/follow_up 入队、消费与计数
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_update_lifecycle() {
    let long_text = "word ".repeat(400);
    let fixture = session_fixture(
        vec![
            assistant(&long_text),
            assistant("steered answer"),
            assistant("follow up answer"),
        ],
        FauxProviderOptions {
            tokens_per_second: Some(60.0),
            ..Default::default()
        },
        None,
    )
    .await;

    // prompt 在后台运行；流式期间 steer + follow_up 入队。
    let session = fixture.session.clone();
    let prompt_task =
        tokio::spawn(async move { session.prompt("start", Default::default()).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(fixture.session.is_streaming());

    fixture
        .session
        .steer("steer note", None)
        .await
        .expect("steer");
    fixture
        .session
        .follow_up("later", None)
        .await
        .expect("follow_up");

    assert_eq!(fixture.session.get_steering_messages(), vec!["steer note"]);
    assert_eq!(fixture.session.get_follow_up_messages(), vec!["later"]);
    assert_eq!(fixture.session.pending_message_count(), 2);

    // queue_update 事件内容（依次入队）。
    let queue_updates: Vec<Value> = fixture
        .events_of_type("queue_update")
        .iter()
        .map(|e| serde_json::to_value(e).expect("serialize"))
        .collect();
    assert!(
        queue_updates
            .iter()
            .any(|q| q["steering"] == serde_json::json!(["steer note"])
                && q["followUp"] == serde_json::json!([])),
        "queue updates: {queue_updates:?}"
    );
    assert!(
        queue_updates
            .iter()
            .any(|q| q["steering"] == serde_json::json!(["steer note"])
                && q["followUp"] == serde_json::json!(["later"])),
        "queue updates: {queue_updates:?}"
    );

    prompt_task.await.expect("prompt task").expect("prompt");
    fixture.session.wait_for_idle().await;

    // 全部消费完毕；消息含 steering 与 follow-up 的用户消息。
    assert_eq!(fixture.session.pending_message_count(), 0);
    let texts: Vec<String> = fixture
        .session
        .messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::User(user) => {
                Some(pir_ai::utils::text::content_text_user(&user.content, ""))
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["start", "steer note", "later"]);
}

// ---------------------------------------------------------------------------
// auto-retry：瞬态错误 → auto_retry_start/end → 成功
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_retry_recovers_from_transient_error() {
    let fixture = session_fixture(
        vec![error_step("429 too many requests"), assistant("recovered")],
        FauxProviderOptions::default(),
        Some(r#"{"retry": {"baseDelayMs": 10}}"#),
    )
    .await;

    fixture
        .session
        .prompt("hello", PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    let retry_start = fixture.events_of_type("auto_retry_start");
    assert_eq!(retry_start.len(), 1);
    let start = serde_json::to_value(&retry_start[0]).expect("serialize");
    assert_eq!(start["attempt"], 1);
    assert_eq!(start["maxAttempts"], 3);
    assert!(start["delayMs"].as_u64().is_some());
    assert!(start["errorMessage"]
        .as_str()
        .expect("errorMessage")
        .contains("429"));

    let retry_end = fixture.events_of_type("auto_retry_end");
    assert_eq!(retry_end.len(), 1);
    let end = serde_json::to_value(&retry_end[0]).expect("serialize");
    assert_eq!(end["success"], true);
    assert_eq!(end["attempt"], 1);

    assert_eq!(
        fixture.session.get_last_assistant_text().as_deref(),
        Some("recovered")
    );
    assert_eq!(fixture.provider.call_count(), 2);
}

/// auto-retry 用尽的最终失败：auto_retry_end success:false + finalError。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_retry_exhausted_reports_final_error() {
    let fixture = session_fixture(
        vec![
            error_step("429 too many requests"),
            error_step("429 too many requests"),
            error_step("429 too many requests"),
            error_step("429 too many requests"),
        ],
        FauxProviderOptions::default(),
        Some(r#"{"retry": {"baseDelayMs": 10}}"#),
    )
    .await;

    fixture
        .session
        .prompt("hello", PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    let retry_end = fixture.events_of_type("auto_retry_end");
    assert_eq!(retry_end.len(), 1);
    let end = serde_json::to_value(&retry_end[0]).expect("serialize");
    assert_eq!(end["success"], false);
    assert_eq!(end["attempt"], 3);
    assert!(end["finalError"]
        .as_str()
        .expect("finalError")
        .contains("429"));
    assert_eq!(fixture.provider.call_count(), 4);
}

// ---------------------------------------------------------------------------
// bash：流式期间暂存 pending，下个 prompt 前 flush（agent-session.ts:2851）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_result_flushes_before_next_prompt() {
    let long_text = "word ".repeat(400);
    let fixture = session_fixture(
        vec![assistant(&long_text), assistant("second answer")],
        FauxProviderOptions {
            tokens_per_second: Some(60.0),
            ..Default::default()
        },
        None,
    )
    .await;

    let session = fixture.session.clone();
    let prompt_task = tokio::spawn(async move { session.prompt("one", Default::default()).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(fixture.session.is_streaming());

    // 流式期间执行 bash：结果进 pending 队列，不进消息状态。
    let result = fixture
        .session
        .execute_bash("printf 'bash-out'", Default::default())
        .await
        .expect("execute bash");
    assert_eq!(result.output, "bash-out");
    assert!(fixture.session.has_pending_bash_messages());
    assert!(
        !fixture
            .session
            .messages()
            .iter()
            .any(|m| matches!(m, AgentMessage::BashExecution(_))),
        "bash result must stay pending while streaming"
    );

    prompt_task.await.expect("prompt task").expect("prompt");
    fixture.session.wait_for_idle().await;

    // run 结束的 finally 即 flush（agent-session.ts:1068-1072）：pending 清空、
    // bashExecution 进入消息状态，且位于第二个 user 消息之前。
    assert!(!fixture.session.has_pending_bash_messages());

    fixture
        .session
        .prompt("two", PromptOptions::default())
        .await
        .expect("second prompt");
    fixture.session.wait_for_idle().await;

    assert!(!fixture.session.has_pending_bash_messages());
    let roles: Vec<&str> = fixture
        .session
        .messages()
        .iter()
        .map(|m| match m {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::BashExecution(_) => "bashExecution",
            _ => "other",
        })
        .collect();
    let bash_index = roles
        .iter()
        .position(|r| *r == "bashExecution")
        .expect("bashExecution message");
    let second_user = roles
        .iter()
        .enumerate()
        .filter(|(_, r)| **r == "user")
        .map(|(i, _)| i)
        .nth(1)
        .expect("second user message");
    assert!(bash_index < second_user, "roles: {roles:?}");
}
