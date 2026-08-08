//! T15 W3 action-binding tests: all 24 API methods through the real host
//! chain (inline factory → NativeExtensionHost → adapter →
//! SessionHostActions → AgentSession).
//!
//! Upstream anchors: agent-session.ts:1429-1503 (sendMessage/
//! sendUserMessage), :2356-2443 (bindCore mapping), :2454-2545
//! (_refreshToolRegistry), wrapper.ts:17-37 (addedToolNames),
//! agent-session-services.ts:81-127 (flag application), exec.ts:34-106
//! (exec).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pir::core::agent_session::AgentSession;
use pir::core::extension_actions::bind_session_actions;
use pir_ext_host::api::{DeliverAs, ExtensionApi, SendMessageOptions};
use pir_ext_host::host::NativeExtensionHost;
use pir_ext_host::loader::{ExtensionFactory, InlineExtension};
use pir_ext_host::types as ext;
use pir_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pir-ext-w3-test-{}-{id}", std::process::id()));
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

/// Slot for the extension's `pi` handle, captured by the factory so tests
/// can invoke API methods after bind.
type ApiSlot = Arc<Mutex<Option<ExtensionApi>>>;

fn api_slot() -> ApiSlot {
    Arc::new(Mutex::new(None))
}

fn capture_api(slot: ApiSlot) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api| {
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(api.clone());
        Box::pin(async { Ok(()) })
    });
    InlineExtension::Anonymous(factory)
}

/// Host + stashed api + (optionally) extra registrations.
async fn host_with_api(registrations: Vec<InlineExtension>) -> (NativeExtensionHost, ApiSlot) {
    let slot = api_slot();
    let mut all = vec![capture_api(slot.clone())];
    all.extend(registrations);
    let host = NativeExtensionHost::new("/w3-cwd");
    let errors = host.load_inline(&all).await;
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    (host, slot)
}

fn slot_api(slot: &ApiSlot) -> ExtensionApi {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("factory ran")
}

struct SessionFixture {
    session: AgentSession,
    provider: Arc<FauxProvider>,
    _tmp: TempDir,
}

/// Full pipeline with the real host AND bound actions (the app.rs W3
/// wiring: create_agent_session + bind_session_actions).
async fn session_fixture(
    responses: Vec<FauxResponseStep>,
    host: NativeExtensionHost,
    provider_options: FauxProviderOptions,
) -> SessionFixture {
    session_fixture_full(responses, host, provider_options, None).await
}

/// `models_json` 可选写入 agent_dir/models.json（provider 覆盖/恢复测试）。
async fn session_fixture_full(
    responses: Vec<FauxResponseStep>,
    host: NativeExtensionHost,
    provider_options: FauxProviderOptions,
    models_json: Option<&str>,
) -> SessionFixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    if let Some(models_json) = models_json {
        std::fs::write(agent_dir.join("models.json"), models_json).expect("models.json");
    }

    let mut options = provider_options;
    if options.models.is_none() {
        options.models = Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]);
    }
    let provider = FauxProvider::new(options);
    provider.set_responses(responses);
    let model = provider.get_model(None).expect("faux model");

    let model_runtime = pir::core::model_runtime::ModelRuntime::create(
        pir::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: pir::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
            ..Default::default()
        },
    )
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux provider");

    let services = pir::core::agent_session_services::create_agent_session_services(
        pir::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");

    let session_manager = Arc::new(Mutex::new(
        pir::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            pir::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));

    let host = Arc::new(host);
    let created = pir::sdk::create_agent_session(pir::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host.clone()),
        ..Default::default()
    })
    .await
    .expect("create session");

    bind_session_actions(&host, &created.session).await;

    SessionFixture {
        session: created.session,
        provider,
        _tmp: tmp,
    }
}

fn text_step(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxAssistantOptions::default()).into()
}

/// Wait until `cond` holds (streaming-state tests), ~2s budget.
async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("condition not met in time");
}

// ---------------------------------------------------------------------------
// sendMessage（agent-session.ts:1429-1463）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_send_message_idle_without_trigger_turn_appends_only() {
    let (host, slot) = host_with_api(Vec::new()).await;
    let fixture = session_fixture(Vec::new(), host, FauxProviderOptions::default()).await;

    slot_api(&slot)
        .send_message(
            json!({"customType": "note", "content": "hello", "display": false}),
            None,
        )
        .expect("send_message");
    // spawn 调度的动作跑完（无 triggerTurn → 不产生 LLM 调用）。
    wait_until(|| {
        fixture
            .session
            .messages()
            .iter()
            .any(|m| matches!(m, pir_agent::messages::AgentMessage::Custom(_)))
    })
    .await;
    assert_eq!(fixture.provider.pending_response_count(), 0);
    assert_eq!(fixture.session.pending_message_count(), 0);
    let messages = fixture.session.messages();
    let custom = messages
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .find(|v| v["role"] == "custom")
        .unwrap_or_else(|| {
            panic!(
                "custom message missing in {:?}",
                messages
                    .iter()
                    .map(|m| serde_json::to_value(m).ok())
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(custom["customType"], "note");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_send_message_next_turn_queues_for_next_prompt() {
    let (host, slot) = host_with_api(Vec::new()).await;
    let fixture = session_fixture(
        vec![text_step("answer")],
        host,
        FauxProviderOptions::default(),
    )
    .await;

    slot_api(&slot)
        .send_message(
            json!({"customType": "aside", "display": false}),
            Some(SendMessageOptions {
                trigger_turn: None,
                deliver_as: Some(DeliverAs::NextTurn),
            }),
        )
        .expect("send_message");
    // nextTurn：不入当前消息流。
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(fixture
        .session
        .messages()
        .iter()
        .all(|m| { !matches!(m, pir_agent::messages::AgentMessage::Custom(_)) }));

    fixture
        .session
        .prompt("go", pir::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    // 下轮 prompt 时入列（agent-session.ts:1435-1437 + :1219-1220）。
    let has_aside = fixture.session.messages().iter().any(|m| {
        serde_json::to_value(m)
            .map(|v| v["customType"] == "aside")
            .unwrap_or(false)
    });
    assert!(has_aside, "nextTurn message joined the prompt batch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_send_message_streaming_follow_up_queues_into_run() {
    // 慢响应制造 streaming 窗口。
    let long_text = "x".repeat(800);
    let (host, slot) = host_with_api(Vec::new()).await;
    let fixture = session_fixture(
        vec![text_step(&long_text), text_step("second turn")],
        host,
        FauxProviderOptions {
            tokens_per_second: Some(50.0),
            ..Default::default()
        },
    )
    .await;

    let session = fixture.session.clone();
    let prompt_task = tokio::spawn(async move {
        session
            .prompt("start", pir::core::agent_session::PromptOptions::default())
            .await
    });
    let streaming_session = fixture.session.clone();
    wait_until(|| streaming_session.is_streaming()).await;

    slot_api(&slot)
        .send_message(
            json!({"customType": "mid-run", "display": false}),
            Some(SendMessageOptions {
                trigger_turn: None,
                deliver_as: Some(DeliverAs::FollowUp),
            }),
        )
        .expect("send_message");

    prompt_task.await.expect("prompt task").expect("prompt");
    fixture.session.wait_for_idle().await;

    // followUp 分支：消息进 agent 队列并触发第二轮 LLM 调用。
    let has_mid_run = fixture.session.messages().iter().any(|m| {
        serde_json::to_value(m)
            .map(|v| v["customType"] == "mid-run")
            .unwrap_or(false)
    });
    assert!(has_mid_run, "followUp message delivered");
}

// ---------------------------------------------------------------------------
// sendUserMessage（agent-session.ts:1472-1503 + :1160-1164）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_send_user_message_streaming_without_deliver_as_errors() {
    let long_text = "y".repeat(800);
    let (host, slot) = host_with_api(Vec::new()).await;
    let ext_errors = Arc::new(Mutex::new(Vec::new()));
    let sink = ext_errors.clone();
    let _unsub = host.on_error(Arc::new(move |e| {
        sink.lock().unwrap_or_else(|e2| e2.into_inner()).push(e);
    }));

    let fixture = session_fixture(
        vec![text_step(&long_text)],
        host,
        FauxProviderOptions {
            tokens_per_second: Some(50.0),
            ..Default::default()
        },
    )
    .await;

    let session = fixture.session.clone();
    let prompt_task = tokio::spawn(async move {
        session
            .prompt("start", pir::core::agent_session::PromptOptions::default())
            .await
    });
    let streaming_session = fixture.session.clone();
    wait_until(|| streaming_session.is_streaming()).await;

    slot_api(&slot)
        .send_user_message(json!("mid-run text"), None)
        .expect("call itself is queued");
    wait_until(|| {
        !ext_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    })
    .await;

    // 中止慢响应，让 prompt_task 收尾。
    fixture.session.abort().await;
    let _ = prompt_task.await;

    let errors = ext_errors.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(errors[0].event, "send_user_message");
    assert!(
        errors[0].error.contains("Agent is already processing"),
        "error: {}",
        errors[0].error
    );
}

// ---------------------------------------------------------------------------
// setActiveTools / getAllTools（agent-session.ts:926-941, 906-913）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_set_active_tools_ignores_unknown_and_rebuilds_prompt() {
    let (host, slot) = host_with_api(Vec::new()).await;
    let fixture = session_fixture(Vec::new(), host, FauxProviderOptions::default()).await;
    let api = slot_api(&slot);

    let before = fixture.session.system_prompt();
    api.set_active_tools(vec!["read".to_owned(), "bogus".to_owned()])
        .expect("set_active_tools");

    // 未注册名静默忽略（agent-session.ts:930-936）。
    assert_eq!(api.get_active_tools().unwrap(), ["read"]);
    // system prompt 重建：bash 的 guideline 退出、read 的 snippet 保留。
    let after = fixture.session.system_prompt();
    assert_ne!(before, after);
    assert!(after.contains("Read file contents"));
    assert!(
        !after.contains("Execute bash commands"),
        "bash snippet must leave the prompt"
    );

    // getAllTools：全部注册工具（含未激活的），带 sourceInfo。
    let all = api.get_all_tools().unwrap();
    let read = all.iter().find(|t| t["name"] == "read").expect("read tool");
    assert_eq!(read["sourceInfo"]["source"], "builtin");
    assert!(all.iter().any(|t| t["name"] == "grep"));
}

// ---------------------------------------------------------------------------
// addedToolNames（wrapper.ts:17-37）
// ---------------------------------------------------------------------------

fn register_tool(api: &ExtensionApi, name: &str, execute: ext::ToolExecuteFn) {
    api.register_tool(ext::ToolDefinition {
        name: name.to_owned(),
        label: name.to_owned(),
        description: format!("{name} tool"),
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute,
        render_call: None,
        render_result: None,
    })
    .expect("register tool");
}

fn tool_call_step(name: &str, args: Value) -> FauxResponseStep {
    faux_assistant_message(
        faux_tool_call(name, args.as_object().cloned().unwrap_or_default(), None),
        FauxAssistantOptions::default(),
    )
    .into()
}

fn tool_result_json(session: &AgentSession) -> Value {
    session
        .messages()
        .into_iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .find(|v| v["role"] == "toolResult")
        .expect("toolResult message")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_added_tool_names_pure_addition_branch() {
    let (host, slot) = host_with_api(Vec::new()).await;
    // activator 在执行期间注册 extra：registerTool 的 refresh 把新名加入
    // active 集（agent-session.ts:2536-2541 的 new-names 分支），构成
    // wrapper.ts:28-34 的纯增量场景。
    let host = host;
    let slot2 = slot.clone();
    let errors = host
        .load_inline(&[InlineExtension::Anonymous(Arc::new(move |api| {
            let slot3 = slot2.clone();
            register_tool(
                &api,
                "activator",
                Arc::new(move |_req, _ctx| {
                    let slot4 = slot3.clone();
                    Box::pin(async move {
                        let api = slot4
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                            .expect("api");
                        register_tool(
                            &api,
                            "extra",
                            Arc::new(|_req, _ctx| {
                                Box::pin(async { Ok(pir_agent::types::AgentToolResult::default()) })
                            }),
                        );
                        Ok(pir_agent::types::AgentToolResult {
                            content: vec![pir_ai::types::ToolResultContent::Text(
                                pir_ai::types::TextContent {
                                    text: "activated".to_owned(),
                                    text_signature: None,
                                },
                            )],
                            ..Default::default()
                        })
                    })
                }),
            );
            Box::pin(async { Ok(()) })
        }))])
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let fixture = session_fixture(
        vec![tool_call_step("activator", json!({})), text_step("done")],
        host,
        FauxProviderOptions::default(),
    )
    .await;

    fixture
        .session
        .prompt("go", pir::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    // 纯增量：addedToolNames 带 extra（wrapper.ts:28-34）。
    let result = tool_result_json(&fixture.session);
    assert_eq!(result["addedToolNames"], json!(["extra"]));
    assert!(fixture
        .session
        .get_active_tool_names()
        .contains(&"extra".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_added_tool_names_suppressed_when_tools_removed() {
    let (host, slot) = host_with_api(Vec::new()).await;
    let host = host;
    let slot2 = slot.clone();
    let errors = host
        .load_inline(&[InlineExtension::Anonymous(Arc::new(move |api| {
            let slot3 = slot2.clone();
            register_tool(
                &api,
                "swapper",
                Arc::new(move |_req, _ctx| {
                    let slot4 = slot3.clone();
                    Box::pin(async move {
                        let api = slot4
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                            .expect("api");
                        // 有移除（read 被拿掉）的集合变更 → 不附加 addedToolNames
                        //（wrapper.ts:26-27）。
                        api.set_active_tools(vec!["swapper".to_owned(), "other".to_owned()])
                            .expect("set_active_tools");
                        Ok(pir_agent::types::AgentToolResult::default())
                    })
                }),
            );
            register_tool(
                &api,
                "other",
                Arc::new(|_req, _ctx| {
                    Box::pin(async { Ok(pir_agent::types::AgentToolResult::default()) })
                }),
            );
            Box::pin(async { Ok(()) })
        }))])
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let fixture = session_fixture(
        vec![tool_call_step("swapper", json!({})), text_step("done")],
        host,
        FauxProviderOptions::default(),
    )
    .await;

    fixture
        .session
        .prompt("go", pir::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    let result = tool_result_json(&fixture.session);
    assert!(
        result.get("addedToolNames").is_none() || result["addedToolNames"].is_null(),
        "no addedToolNames when the active set lost tools: {result}"
    );
}

// ---------------------------------------------------------------------------
// registerProvider / unregisterProvider
// （runner.ts:349-407 + custom-provider.md:190-217）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_register_provider_queue_flush_runtime_and_unregister() {
    // 加载期（pre-bind）注册排队。
    let queued = InlineExtension::Anonymous(Arc::new(|api| {
        Box::pin(async move {
            api.register_provider(
                "ext-proxy",
                json!({
                    "baseUrl": "https://proxy.invalid",
                    "apiKey": "sk-test",
                    "api": "openai-completions",
                    "models": [{
                        "id": "proxy-1",
                        "name": "Proxy One",
                        "reasoning": false,
                        "input": ["text"],
                        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                        "contextWindow": 8192,
                        "maxTokens": 1024
                    }]
                }),
            )
            .await
            .expect("queued registration");
            Ok(())
        })
    }));
    let (host, slot) = host_with_api(vec![queued]).await;
    // models.json 定义的 base-p 扮演「内建」provider（覆盖/恢复用例）。
    let fixture = session_fixture_full(
        Vec::new(),
        host,
        FauxProviderOptions::default(),
        Some(
            r#"{"providers":{"base-p":{"name":"Base","baseUrl":"https://base.invalid","api":"openai-completions","apiKey":"sk-base","models":[{"id":"base-1","name":"Base One","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":8192,"maxTokens":1024}]}}}"#,
        ),
    )
    .await;
    let runtime = fixture.session.model_runtime().clone();

    // bind 冲刷：模型立即可见。
    assert!(
        runtime.get_model("ext-proxy", "proxy-1").is_some(),
        "queued provider flushed on bind"
    );

    // 运行期注册立即生效（runner.ts:387-393）。
    slot_api(&slot)
        .register_provider(
            "ext-proxy-2",
            json!({
                "baseUrl": "https://proxy2.invalid",
                "apiKey": "sk-test",
                "api": "openai-completions",
                "models": [{
                    "id": "proxy-2",
                    "name": "Proxy Two",
                    "reasoning": false,
                    "input": ["text"],
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                    "contextWindow": 8192,
                    "maxTokens": 1024
                }]
            }),
        )
        .await
        .expect("runtime registration");
    assert!(runtime.get_model("ext-proxy-2", "proxy-2").is_some());

    // unregister：移除动态模型（custom-provider.md:190-217）。
    slot_api(&slot)
        .unregister_provider("ext-proxy-2")
        .await
        .expect("unregister");
    assert!(runtime.get_model("ext-proxy-2", "proxy-2").is_none());
    assert!(runtime.get_model("ext-proxy", "proxy-1").is_some());

    // 覆盖 models.json 定义的 provider：models 全量替换（types.ts:1437）；
    // unregister 后恢复内建（custom-provider.md:190-217）。
    assert!(runtime.get_model("base-p", "base-1").is_some());
    slot_api(&slot)
        .register_provider(
            "base-p",
            json!({
                "baseUrl": "https://override.invalid",
                "apiKey": "sk-test",
                "api": "openai-completions",
                "models": [{
                    "id": "override-1",
                    "name": "Override",
                    "reasoning": false,
                    "input": ["text"],
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                    "contextWindow": 8192,
                    "maxTokens": 1024
                }]
            }),
        )
        .await
        .expect("override");
    assert!(runtime.get_model("base-p", "override-1").is_some());
    assert!(
        runtime.get_model("base-p", "base-1").is_none(),
        "extension models replace the provider's existing models"
    );
    slot_api(&slot)
        .unregister_provider("base-p")
        .await
        .expect("unregister base-p");
    assert!(runtime.get_model("base-p", "override-1").is_none());
    assert!(
        runtime.get_model("base-p", "base-1").is_some(),
        "built-in models restored after unregister"
    );
}

// ---------------------------------------------------------------------------
// flags（agent-session-services.ts:81-127 → pir 侧 helper）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w3_flag_values_registered_applied_unknown_errors() {
    let flags_ext = inline_with_flags();
    let host = NativeExtensionHost::new("/w3-cwd");
    let errors = host.load_inline(&[flags_ext]).await;
    assert!(errors.is_empty());

    // 已注册：boolean 置 true（忽略给定值），string 写入值。
    let diagnostics = pir::core::agent_session_services::apply_extension_flag_values(
        &host,
        &[
            (
                "verbose".to_owned(),
                pir::cli::args::UnknownFlagValue::Boolean(true),
            ),
            (
                "output".to_owned(),
                pir::cli::args::UnknownFlagValue::String("file.json".to_owned()),
            ),
        ],
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(
        host.runtime().get_flag_value("verbose"),
        Some(ext::FlagValue::Boolean(true))
    );
    assert_eq!(
        host.runtime().get_flag_value("output"),
        Some(ext::FlagValue::String("file.json".to_owned()))
    );

    // string flag 缺值 → 报错；未注册 → Unknown option(s)。
    let diagnostics = pir::core::agent_session_services::apply_extension_flag_values(
        &host,
        &[
            (
                "output".to_owned(),
                pir::cli::args::UnknownFlagValue::Boolean(true),
            ),
            (
                "nope".to_owned(),
                pir::cli::args::UnknownFlagValue::Boolean(true),
            ),
        ],
    );
    let messages: Vec<&str> = diagnostics.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        messages,
        [
            "Extension flag \"--output\" requires a value",
            "Unknown option: --nope"
        ]
    );
}

fn inline_with_flags() -> InlineExtension {
    InlineExtension::Anonymous(Arc::new(|api| {
        api.register_flag(
            "verbose",
            Some("Verbose output".to_owned()),
            ext::FlagType::Boolean,
            None,
        )
        .expect("flag");
        api.register_flag("output", None, ext::FlagType::String, None)
            .expect("flag");
        Box::pin(async { Ok(()) })
    }))
}

// ---------------------------------------------------------------------------
// 覆盖内置工具（agent-session.ts:2454-2545 的 Map.set 覆盖序）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_extension_tool_overrides_builtin_definition_and_execution() {
    let (host, _slot) = host_with_api(Vec::new()).await;
    let host = host;
    let errors = host
        .load_inline(&[InlineExtension::Anonymous(Arc::new(|api| {
            register_tool(
                &api,
                "read",
                Arc::new(|_req, _ctx| {
                    Box::pin(async {
                        Ok(pir_agent::types::AgentToolResult {
                            content: vec![pir_ai::types::ToolResultContent::Text(
                                pir_ai::types::TextContent {
                                    text: "ext-read-executed".to_owned(),
                                    text_signature: None,
                                },
                            )],
                            ..Default::default()
                        })
                    })
                }),
            );
            Box::pin(async { Ok(()) })
        }))])
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let fixture = session_fixture(Vec::new(), host, FauxProviderOptions::default()).await;

    // 定义覆盖：getAllTools 的 read 来自扩展（source 非 builtin、描述替换）。
    let all = fixture.session.get_all_tools();
    let read = all.iter().find(|t| t["name"] == "read").expect("read");
    assert_eq!(read["description"], "read tool");
    assert_eq!(read["sourceInfo"]["source"], "inline");
    // promptSnippet 不继承内置（覆盖且无 snippet → prompt 不含内置片段）。
    assert!(
        !fixture
            .session
            .system_prompt()
            .contains("Read file contents"),
        "builtin snippet must not be inherited by the override"
    );

    // 执行覆盖：LLM 调 read → 扩展实现运行。
    fixture.provider.set_responses(vec![
        tool_call_step("read", json!({"path": "x"})),
        text_step("done"),
    ]);
    fixture
        .session
        .prompt("go", pir::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;
    let result = tool_result_json(&fixture.session);
    assert_eq!(result["content"][0]["text"], "ext-read-executed");
}

// ---------------------------------------------------------------------------
// events 总线（跨扩展 pub/sub）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w3_events_bus_cross_extension_pub_sub() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let listener = InlineExtension::Anonymous(Arc::new(move |api| {
        let sink = sink.clone();
        // 退订句柄不调用即保持订阅。
        let _unsub = api.events().on(
            "my-channel",
            Arc::new(move |data| {
                sink.lock().unwrap_or_else(|e| e.into_inner()).push(data);
            }),
        );
        std::mem::forget(_unsub);
        Box::pin(async { Ok(()) })
    }));
    let sender_slot = api_slot();
    let sender = capture_api(sender_slot.clone());

    let host = NativeExtensionHost::new("/w3-cwd");
    let errors = host.load_inline(&[listener, sender]).await;
    assert!(errors.is_empty());

    slot_api(&sender_slot)
        .events()
        .emit("my-channel", json!({"n": 42}));
    assert_eq!(
        received
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_slice(),
        &[json!({"n": 42})]
    );
}

// ---------------------------------------------------------------------------
// getCommands 顺序 + 扩展命令执行
// （agent-session.ts:2332-2355 / :1267-1294）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_get_commands_orders_extension_first_with_suffixes() {
    let cmd_handler =
        || -> ext::CommandHandlerFn { Arc::new(|_args, _ctx| Box::pin(async { Ok(()) })) };
    // 同名冲突来自两个扩展（同扩展内 registerCommand 是 Map.set 覆盖，
    // loader.ts:254-261）。
    let commands_a = InlineExtension::Anonymous(Arc::new(move |api| {
        api.register_command("dup", Some("first".to_owned()), cmd_handler())
            .expect("cmd");
        Box::pin(async { Ok(()) })
    }));
    let commands_b = InlineExtension::Anonymous(Arc::new(move |api| {
        api.register_command("dup", Some("second".to_owned()), cmd_handler())
            .expect("cmd");
        api.register_command("solo", Some("alone".to_owned()), cmd_handler())
            .expect("cmd");
        Box::pin(async { Ok(()) })
    }));
    let (host, _slot) = host_with_api(vec![commands_a, commands_b]).await;
    let fixture = session_fixture(Vec::new(), host, FauxProviderOptions::default()).await;

    let commands = fixture.session.get_commands_info();
    let names: Vec<&str> = commands.iter().filter_map(|c| c["name"].as_str()).collect();
    // 扩展命令在最前（runner.ts:595-629 的 :N 后缀）。
    assert_eq!(&names[..3], ["dup:1", "dup:2", "solo"]);
    assert_eq!(commands[0]["source"], "extension");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_extension_command_executes_via_prompt() {
    let captured = Arc::new(Mutex::new(None));
    let sink = captured.clone();
    let command_ext = InlineExtension::Anonymous(Arc::new(move |api| {
        let sink = sink.clone();
        api.register_command(
            "mycmd",
            None,
            Arc::new(move |args, _ctx| {
                let sink = sink.clone();
                Box::pin(async move {
                    *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(args);
                    Ok(())
                })
            }),
        )
        .expect("cmd");
        Box::pin(async { Ok(()) })
    }));
    let (host, _slot) = host_with_api(vec![command_ext]).await;
    // 无脚本响应：命令路径不应触发 LLM 调用。
    let fixture = session_fixture(Vec::new(), host, FauxProviderOptions::default()).await;

    fixture
        .session
        .prompt(
            "/mycmd alpha beta",
            pir::core::agent_session::PromptOptions::default(),
        )
        .await
        .expect("prompt");
    assert_eq!(
        captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref(),
        Some("alpha beta")
    );
    assert_eq!(fixture.provider.pending_response_count(), 0);
}

// ---------------------------------------------------------------------------
// exec（exec.ts:34-106）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w3_exec_runs_command_and_reports_timeout() {
    let (host, slot) = host_with_api(Vec::new()).await;
    let _fixture = session_fixture(Vec::new(), host, FauxProviderOptions::default()).await;
    let api = slot_api(&slot);

    let result = api
        .exec("echo", &["hello".to_owned()], None)
        .await
        .expect("exec");
    assert_eq!(result.stdout.trim(), "hello");
    assert_eq!(result.code, 0);
    assert!(!result.killed);

    let result = api
        .exec(
            "sleep",
            &["5".to_owned()],
            Some(pir_ext_host::api::ExecOptions {
                timeout: Some(50),
                cwd: None,
            }),
        )
        .await
        .expect("exec timeout");
    assert!(result.killed);

    // 不存在的命令：code 1（exec.ts:98-103 catch 分支）。
    let result = api
        .exec("definitely-not-a-real-binary", &[], None)
        .await
        .expect("exec missing");
    assert_eq!(result.code, 1);
}
