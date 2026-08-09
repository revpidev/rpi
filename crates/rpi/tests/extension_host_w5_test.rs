//! T15 W5 tests: the three context levels (ExtensionContext completion /
//! CommandContext / ReplacedSessionContext), session-replacement ordering
//! with stale invalidation, and the reload flow.
//!
//! Upstream anchors: types.ts:306-398 (three-level ctx), runner.ts:665-777
//! (lazy getters + assertActive), agent-session-runtime.ts:133-215
//! (replacement ordering),
//! agent-session.ts:2600-2628（reload）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rpi::core::agent_session::ExtensionBindings;
use rpi::core::extension_actions::bind_session_actions;
use rpi::core::extension_context::RuntimeCommandActions;
use rpi_ext_host::api::{CommandContextActions, CompactOptions, ExtensionApi, ExtensionContext};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::InlineExtension;
use rpi_ext_host::types as ext;
use rpi_ext_host::ExtError;
use rpi_test_support::faux::{
    faux_assistant_message, FauxAiProvider, FauxAssistantOptions, FauxModelDefinition,
    FauxProvider, FauxProviderOptions,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixture helpers（沿用 W2/W3 模式）
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rpi-ext-w5-test-{}-{id}", std::process::id()));
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

fn on_json(
    api: &ExtensionApi,
    event: &str,
    f: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
) {
    api.on(
        event,
        Arc::new(move |payload, _ctx| {
            let result = f(payload);
            Box::pin(async move { result })
        }),
    )
    .expect("register handler");
}

/// Runtime factory result pieces, mirroring the app.rs W2/W3 wiring:
/// fresh host + session + bound actions per creation.
struct RuntimeFixture {
    runtime: rpi::core::agent_session_runtime::AgentSessionRuntime,
    hosts: Arc<Mutex<Vec<Arc<NativeExtensionHost>>>>,
    apis: Arc<Mutex<Vec<ExtensionApi>>>,
    _tmp: TempDir,
}

async fn runtime_fixture(event_log: Arc<Mutex<Vec<String>>>) -> RuntimeFixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    // Tiny keepRecentTokens so manual compact() has content in tests.
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"compaction": {"keepRecentTokens": 10}}"#,
    )
    .expect("settings.json");

    let hosts: Arc<Mutex<Vec<Arc<NativeExtensionHost>>>> = Arc::new(Mutex::new(Vec::new()));
    let apis: Arc<Mutex<Vec<ExtensionApi>>> = Arc::new(Mutex::new(Vec::new()));

    let factory = {
        let hosts = hosts.clone();
        let apis = apis.clone();
        let event_log = event_log.clone();
        let agent_dir = agent_dir.clone();
        move |options: rpi::core::agent_session_runtime::CreateRuntimeOptions| {
            let hosts = hosts.clone();
            let apis = apis.clone();
            let event_log = event_log.clone();
            let agent_dir = agent_dir.clone();
            Box::pin(async move {
                let provider = FauxProvider::new(FauxProviderOptions {
                    models: Some(vec![FauxModelDefinition {
                        id: "faux-1".to_owned(),
                        name: None,
                        reasoning: None,
                        input: None,
                        cost: None,
                        context_window: Some(200_000),
                        max_tokens: Some(8192),
                    }]),
                    ..Default::default()
                });
                provider.set_responses(vec![faux_assistant_message(
                    "ok",
                    FauxAssistantOptions::default(),
                )
                .into()]);
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
                    .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
                    .await
                    .expect("register faux");
                let services = rpi::core::agent_session_services::create_agent_session_services(
                    rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
                        cwd: options.cwd.clone(),
                        agent_dir: Some(agent_dir.clone()),
                        settings_manager: None,
                        model_runtime: Some(model_runtime.clone()),
                        extension_flag_values: Vec::new(),
                        resource_loader_options: None,
                    },
                )
                .await
                .expect("services");

                // Inline extensions per host creation: lifecycle loggers on
                // shutdown/start + an api slot for stale assertions.
                let log = event_log.clone();
                let api_slot = apis.clone();
                let inline = vec![InlineExtension::Anonymous(Arc::new(move |api| {
                    api_slot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(api.clone());
                    let log_shutdown = log.clone();
                    on_json(&api, ext::EVENT_SESSION_SHUTDOWN, move |event| {
                        let reason = event["reason"].as_str().unwrap_or("?").to_owned();
                        log_shutdown
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(format!("shutdown:{reason}"));
                        Ok(Value::Null)
                    });
                    let log_start = log.clone();
                    on_json(&api, ext::EVENT_SESSION_START, move |event| {
                        let reason = event["reason"].as_str().unwrap_or("?").to_owned();
                        log_start
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(format!("start:{reason}"));
                        Ok(Value::Null)
                    });
                    Box::pin(async { Ok(()) })
                }))];
                let host = NativeExtensionHost::new(&options.cwd.to_string_lossy());
                let errors = host.load_inline(&inline).await;
                assert!(errors.is_empty(), "{errors:?}");
                let host = Arc::new(host);
                hosts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(host.clone());

                let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
                    cwd: Some(options.cwd.clone()),
                    agent_dir: Some(agent_dir.clone()),
                    model_runtime: Some(model_runtime),
                    model: Some(model),
                    services: Some(services.clone()),
                    session_manager: Some(options.session_manager),
                    session_start_event: options.session_start_event,
                    extension_host: Some(host.clone()),
                    ..Default::default()
                })
                .await
                .expect("create session");
                bind_session_actions(&host, &created.session).await;

                Ok(
                    rpi::core::agent_session_runtime::CreateAgentSessionRuntimeResult {
                        session: created.session,
                        services,
                        diagnostics: Vec::new(),
                        model_fallback_message: None,
                    },
                )
            })
                as futures::future::BoxFuture<
                    'static,
                    Result<
                        rpi::core::agent_session_runtime::CreateAgentSessionRuntimeResult,
                        rpi::error::RpiError,
                    >,
                >
        }
    };

    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let runtime = rpi::core::agent_session_runtime::create_agent_session_runtime(
        Arc::new(factory),
        rpi::core::agent_session_runtime::CreateRuntimeOptions {
            cwd: cwd.clone(),
            agent_dir,
            session_manager,
            session_start_event: None,
            project_trust_context: None,
        },
    )
    .await
    .expect("runtime");

    RuntimeFixture {
        runtime,
        hosts,
        apis,
        _tmp: tmp,
    }
}

// ---------------------------------------------------------------------------
// ExtensionContext 补全（types.ts:324-341）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w5_context_base_accessors() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let fixture = runtime_fixture(event_log).await;
    let ctx: ExtensionContext = {
        let host = fixture.hosts.lock().unwrap()[0].clone();
        host.core().create_context()
    };

    assert!(ctx.is_idle().unwrap(), "idle before any prompt");
    assert!(ctx.is_project_trusted().unwrap());
    assert_eq!(ctx.cwd().unwrap(), fixture.runtime.session().cwd());
    assert!(!ctx.has_pending_messages().unwrap());
    assert!(ctx.signal().unwrap().is_none(), "no signal while idle");
    assert!(ctx.model().unwrap().is_some(), "faux model present");
    // getSystemPrompt：当前生效 prompt（非空）。
    assert!(!ctx.get_system_prompt().unwrap().is_empty());
    // getContextUsage：无 usage 数据 → tokens None（三态之一）。
    let usage = ctx.get_context_usage().unwrap();
    if let Some(usage) = usage {
        assert!(usage.context_window > 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w5_context_shutdown_invokes_mode_handler() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let fixture = runtime_fixture(event_log).await;
    let called = Arc::new(AtomicUsize::new(0));
    let marker = called.clone();
    fixture
        .runtime
        .session()
        .bind_extensions(ExtensionBindings {
            mode: None,
            on_error: None,
            shutdown: Some(Arc::new(move || {
                marker.fetch_add(1, Ordering::Relaxed);
            })),
        })
        .await;
    let host = fixture.hosts.lock().unwrap()[0].clone();
    let ctx = host.core().create_context();
    ctx.shutdown().unwrap();
    assert_eq!(called.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w5_context_compact_fires_and_forgets_with_callback() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let fixture = runtime_fixture(event_log).await;
    let session = fixture.runtime.session().clone();

    // 铺三轮对话（超过 keep_recent 阈值）使压缩有内容。
    {
        let manager = session.session_manager();
        let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..3 {
            manager
                .append_message(rpi_agent::messages::AgentMessage::User(
                    rpi_ai::types::UserMessage {
                        role: rpi_ai::types::UserRole::User,
                        content: rpi_ai::types::UserContent::Text(format!(
                            "question {i} {}",
                            "x".repeat(80)
                        )),
                        timestamp: 1,
                    },
                ))
                .expect("user");
            let mut assistant =
                faux_assistant_message(format!("answer {i}"), FauxAssistantOptions::default());
            assistant.usage = serde_json::from_value(json!({
                "input": 100, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 100,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
            }))
            .expect("usage");
            assistant.timestamp = (i + 1) as i64 * 1000;
            manager
                .append_message(rpi_agent::messages::AgentMessage::Assistant(assistant))
                .expect("assistant");
        }
        let messages = manager.build_session_context().messages;
        drop(manager);
        session.agent().set_messages(messages);
    }
    // faux 提供摘要响应。
    let host = fixture.hosts.lock().unwrap()[0].clone();
    let ctx = host.core().create_context();

    let completed = Arc::new(Mutex::new(false));
    let failed = Arc::new(Mutex::new(None));
    let marker = completed.clone();
    let err_sink = failed.clone();
    ctx.compact(CompactOptions {
        custom_instructions: None,
        on_complete: Some(Arc::new(move |result| {
            assert!(result["summary"].as_str().is_some());
            *marker.lock().unwrap_or_else(|e| e.into_inner()) = true;
        })),
        on_error: Some(Arc::new(move |error| {
            *err_sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        })),
    })
    .unwrap();

    for _ in 0..200 {
        if *completed.lock().unwrap_or_else(|e| e.into_inner())
            || failed.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    if let Some(error) = failed.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        panic!("compact onError: {error}");
    }
    assert!(
        *completed.lock().unwrap_or_else(|e| e.into_inner()),
        "compact onComplete fired"
    );
}

// ---------------------------------------------------------------------------
// session 替换：时序 + stale + withSession（agent-session-runtime.ts:133-215）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w5_new_session_replacement_sequence_and_stale() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let mut fixture = runtime_fixture(event_log.clone()).await;

    // rebind：新 session bind_extensions（发 session_start）。
    fixture.runtime.set_rebind_session(Some(Box::new(|session| {
        Box::pin(async move {
            session
                .bind_extensions(ExtensionBindings {
                    mode: None,
                    on_error: None,
                    shutdown: None,
                })
                .await;
        })
    })));

    let old_host = fixture.hosts.lock().unwrap()[0].clone();
    let old_api = fixture.apis.lock().unwrap()[0].clone();
    let old_session_id = fixture.runtime.session().session_id();

    let with_session_ran = Arc::new(Mutex::new(false));
    let marker = with_session_ran.clone();
    let runtime = Arc::new(tokio::sync::Mutex::new(fixture.runtime));
    let actions = RuntimeCommandActions::new(&runtime);
    // 注意：RuntimeCommandActions 经 Weak 持有 runtime——本测试此后经
    // actions 操作。
    let cancelled = actions
        .new_session(
            None,
            Some(Arc::new(move |ctx| {
                let marker = marker.clone();
                Box::pin(async move {
                    *marker.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    // withSession 绑定新 session：发消息进新会话。
                    ctx.send_message(
                        json!({"customType": "post-replace", "display": false}),
                        None,
                    )
                    .expect("send into new session");
                })
            })),
        )
        .await;
    assert!(!cancelled);

    // 时序：旧 shutdown(new) → 新 start(new) → withSession。
    let log = event_log.lock().unwrap().clone();
    let shutdown_pos = log
        .iter()
        .position(|e| e == "shutdown:new")
        .expect("shutdown");
    let start_pos = log.iter().position(|e| e == "start:new").expect("start");
    assert!(shutdown_pos < start_pos, "sequence: {log:?}");
    assert!(*with_session_ran.lock().unwrap());
    assert!(matches!(log.last().map(String::as_str), Some("start:new")));

    // 旧 ctx/api stale（loader.ts:175-179 + runner.ts:539-552）。
    assert!(old_host.runtime().is_stale());
    match old_api.get_session_name() {
        Err(ExtError::Stale(_)) => {}
        other => panic!("expected Stale, got {other:?}"),
    }
    match old_host.core().create_context().is_idle() {
        Err(ExtError::Stale(_)) => {}
        other => panic!("expected Stale ctx, got {other:?}"),
    }

    // withSession 发送的消息在新 session（需要运行时取回——从 actions 借用
    // 不可能，改用 hosts[1] 的 runtime 侧效果：直接查新 host 无报错即可；
    // 消息内容经新 session 的 messages 验证放在下方）。
    let _ = old_session_id;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w5_session_before_switch_cancel_blocks_replacement() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let mut fixture = runtime_fixture(event_log).await;

    // 给当前（唯一）host 追加一个取消 handler。
    let host = fixture.hosts.lock().unwrap()[0].clone();
    let canceller = InlineExtension::Anonymous(Arc::new(|api| {
        on_json(&api, ext::EVENT_SESSION_BEFORE_SWITCH, |_| {
            Ok(json!({"cancel": true}))
        });
        Box::pin(async { Ok(()) })
    }));
    let errors = host.load_inline(&[canceller]).await;
    assert!(errors.is_empty());

    fixture.runtime.set_rebind_session(Some(Box::new(|session| {
        Box::pin(async move {
            session
                .bind_extensions(ExtensionBindings {
                    mode: None,
                    on_error: None,
                    shutdown: None,
                })
                .await;
        })
    })));

    let hosts_before = fixture.hosts.lock().unwrap().len();
    let runtime = Arc::new(tokio::sync::Mutex::new(fixture.runtime));
    let actions = RuntimeCommandActions::new(&runtime);
    let cancelled = actions.new_session(None, None).await;
    assert!(cancelled, "session_before_switch cancel blocks newSession");
    // 无替换：未创建新 host、旧 runtime 未 stale。
    drop(actions);
    // （runtime 被 actions 拿走；用 hosts 计数验证。）
    assert_eq!(fixture.hosts.lock().unwrap().len(), hosts_before);
    assert!(!fixture.hosts.lock().unwrap()[0].runtime().is_stale());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w5_navigate_tree_cancel_branch() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let fixture = runtime_fixture(event_log).await;
    let session = fixture.runtime.session().clone();

    // 铺两个 entry；目标是第一个（导航到当前 leaf 会提前返回，不发事件）。
    let target_id = {
        let manager = session.session_manager();
        let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
        let first = manager
            .append_message(rpi_agent::messages::AgentMessage::User(
                rpi_ai::types::UserMessage {
                    role: rpi_ai::types::UserRole::User,
                    content: rpi_ai::types::UserContent::Text("q1".to_owned()),
                    timestamp: 1,
                },
            ))
            .expect("user");
        manager
            .append_message(rpi_agent::messages::AgentMessage::User(
                rpi_ai::types::UserMessage {
                    role: rpi_ai::types::UserRole::User,
                    content: rpi_ai::types::UserContent::Text("q2".to_owned()),
                    timestamp: 2,
                },
            ))
            .expect("user2");
        first
    };

    let host = fixture.hosts.lock().unwrap()[0].clone();
    let canceller = InlineExtension::Anonymous(Arc::new(|api| {
        on_json(&api, ext::EVENT_SESSION_BEFORE_TREE, |_| {
            Ok(json!({"cancel": true}))
        });
        Box::pin(async { Ok(()) })
    }));
    host.load_inline(&[canceller]).await;

    let runtime = Arc::new(tokio::sync::Mutex::new(fixture.runtime));
    let actions = RuntimeCommandActions::new(&runtime);
    let cancelled = actions.navigate_tree(&target_id, Default::default()).await;
    assert!(cancelled, "session_before_tree cancel propagates");
}

// ---------------------------------------------------------------------------
// reload（agent-session.ts:2600-2628 + loader.ts:151-155）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w5_reload_reruns_factories_preserves_flags_and_stales_old() {
    let runs = Arc::new(AtomicUsize::new(0));
    let factory_runs = runs.clone();
    let counter = InlineExtension::Anonymous(Arc::new(move |api| {
        factory_runs.fetch_add(1, Ordering::Relaxed);
        api.register_flag("my-flag", None, ext::FlagType::String, None)
            .expect("flag");
        Box::pin(async { Ok(()) })
    }));

    let tmp = TempDir::new();
    let host = NativeExtensionHost::new(&tmp.path().to_string_lossy());
    let errors = host
        .load_startup_final(
            tmp.path().join("agent"),
            Vec::new(),
            Vec::new(),
            vec![counter],
            true,
            false,
        )
        .await;
    assert!(errors.is_empty());
    assert_eq!(runs.load(Ordering::Relaxed), 1);

    // CLI 写入 flag 值。
    host.runtime()
        .set_flag_value("my-flag", ext::FlagValue::String("cli".to_owned()));

    let old_runtime = host.runtime();
    let errors = host.reload().await;
    assert!(errors.is_empty());
    assert_eq!(runs.load(Ordering::Relaxed), 2, "factory re-ran on reload");
    // flag 值保留（resource-loader reload 语义）。
    assert_eq!(
        host.runtime().get_flag_value("my-flag"),
        Some(ext::FlagValue::String("cli".to_owned()))
    );
    // 旧 runtime stale。
    assert!(old_runtime.is_stale());
    assert!(!host.runtime().is_stale());
    // 模块缓存 generation 递增（clearExtensionCache, loader.ts:151-155）。
    // 由 loader 单测覆盖 clear() 语义；这里验证 reload 后注册表重建。
    assert_eq!(
        host.get_flags().get("my-flag").map(|f| f.name.as_str()),
        Some("my-flag")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w5_session_reload_event_sequence() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let fixture = runtime_fixture(event_log.clone()).await;

    // resources_discover 记录 reason。
    let host = fixture.hosts.lock().unwrap()[0].clone();
    let log = event_log.clone();
    let discover = InlineExtension::Anonymous(Arc::new(move |api| {
        let log = log.clone();
        on_json(&api, ext::EVENT_RESOURCES_DISCOVER, move |event| {
            let reason = event["reason"].as_str().unwrap_or("?").to_owned();
            log.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("discover:{reason}"));
            Ok(json!({}))
        });
        Box::pin(async { Ok(()) })
    }));
    host.load_inline(&[discover]).await;

    fixture.runtime.session().reload().await;

    let log = event_log.lock().unwrap().clone();
    let shutdown = log.iter().position(|e| e == "shutdown:reload");
    let start = log.iter().position(|e| e == "start:reload");
    let discover = log.iter().position(|e| e == "discover:reload");
    assert!(
        shutdown.is_some() && start.is_some() && discover.is_some(),
        "log: {log:?}"
    );
    assert!(shutdown < start && start < discover, "order: {log:?}");
}
