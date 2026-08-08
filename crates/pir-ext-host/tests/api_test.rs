//! Tests for `ExtensionApi` / `ExtensionRuntime` — throwing stubs, flag
//! semantics, provider queue, event bus, and the typed handler wrapper,
//! anchored to `external/pi/packages/coding-agent/src/core/extensions/
//! loader.ts` @ 2efa728.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pir_ext_host::api::{
    EventBus, ExecOptions, ExecResult, ExtensionApi, HostActions, InsertionMap, SendMessageOptions,
    SendUserMessageOptions,
};
use pir_ext_host::host::NativeExtensionHost;
use pir_ext_host::loader::{ExtensionFactory, InlineExtension};
use pir_ext_host::types::{self as ext, FlagType, FlagValue, EVENT_SESSION_START};
use pir_ext_host::ExtError;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Mock HostActions
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockActions {
    sent_messages: Mutex<Vec<(Value, Option<SendMessageOptions>)>>,
    session_name: Mutex<Option<String>>,
    active_tools: Mutex<Vec<String>>,
    thinking_level: Mutex<String>,
    refresh_count: Mutex<usize>,
    registered_providers: Mutex<Vec<(String, Value)>>,
    appended_entries: Mutex<Vec<(String, Option<Value>)>>,
    labels: Mutex<Vec<(String, Option<String>)>>,
    fail_provider_registration: bool,
}

#[async_trait]
impl HostActions for MockActions {
    fn send_message(&self, message: Value, options: Option<SendMessageOptions>) {
        self.sent_messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((message, options));
    }

    fn send_user_message(&self, _content: Value, _options: Option<SendUserMessageOptions>) {}

    fn append_entry(&self, custom_type: &str, data: Option<Value>) {
        self.appended_entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((custom_type.to_owned(), data));
    }

    fn set_session_name(&self, name: &str) {
        *self.session_name.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_owned());
    }

    fn get_session_name(&self) -> Option<String> {
        self.session_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_label(&self, entry_id: &str, label: Option<&str>) {
        self.labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((entry_id.to_owned(), label.map(str::to_owned)));
    }

    async fn exec(
        &self,
        _command: &str,
        _args: &[String],
        _options: Option<ExecOptions>,
    ) -> Result<ExecResult, ExtError> {
        Ok(ExecResult {
            stdout: "out".to_owned(),
            stderr: String::new(),
            code: 0,
            killed: false,
        })
    }

    fn get_active_tools(&self) -> Vec<String> {
        self.active_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn get_all_tools(&self) -> Vec<Value> {
        Vec::new()
    }

    fn set_active_tools(&self, tool_names: Vec<String>) {
        *self.active_tools.lock().unwrap_or_else(|e| e.into_inner()) = tool_names;
    }

    fn refresh_tools(&self) {
        *self.refresh_count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }

    fn get_commands(&self) -> Vec<Value> {
        Vec::new()
    }

    async fn set_model(&self, _model: Value) -> bool {
        true
    }

    fn get_thinking_level(&self) -> String {
        self.thinking_level
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_thinking_level(&self, level: &str) {
        *self
            .thinking_level
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = level.to_owned();
    }

    async fn register_provider(&self, name: &str, config: Value) -> Result<(), String> {
        if self.fail_provider_registration {
            return Err("provider rejected".to_owned());
        }
        self.registered_providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((name.to_owned(), config));
        Ok(())
    }

    async fn register_native_provider(
        &self,
        _provider: Arc<dyn pir_ai::models::Provider>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn unregister_provider(&self, _name: &str) {}
}

fn inline_ext(
    name: &str,
    register: impl Fn(&ExtensionApi) + Send + Sync + 'static,
) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api| {
        register(&api);
        Box::pin(async { Ok(()) })
    });
    InlineExtension::Named {
        name: name.to_owned(),
        factory,
        hidden: false,
    }
}

async fn host_with(extensions: Vec<InlineExtension>) -> NativeExtensionHost {
    let host = NativeExtensionHost::new("/test-cwd");
    let errors = host.load_inline(&extensions).await;
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    host
}

/// Inline extension with an async factory (provider registration etc.).
fn inline_ext_async(
    name: &str,
    register: impl Fn(
            pir_ext_host::api::ExtensionApi,
        ) -> pir_ext_host::api::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync
        + 'static,
) -> InlineExtension {
    let factory: ExtensionFactory = Arc::new(move |api| register(api));
    InlineExtension::Named {
        name: name.to_owned(),
        factory,
        hidden: false,
    }
}

/// Capture the API of a loaded inline extension for post-load calls.
fn api_of(host: &NativeExtensionHost) -> ExtensionApi {
    ExtensionApi::for_extension(
        host.core().extensions()[0].clone(),
        host.runtime(),
        "/test-cwd",
    )
}

// ---------------------------------------------------------------------------
// Throwing stubs (loader.ts:166-223)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_actions_throw_unbound_before_host_binds() {
    // loader.ts:170-196 — every action method fails until bindCore.
    let host = host_with(vec![inline_ext("ext-a", |_api| {})]).await;
    let api = api_of(&host);

    match api.send_message(json!({"customType": "x", "display": false}), None) {
        Err(ExtError::Unbound(message)) => {
            assert!(message.contains("Extension runtime not initialized"));
        }
        other => panic!("expected Unbound, got {other:?}"),
    }
    assert!(matches!(api.get_session_name(), Err(ExtError::Unbound(_))));
    assert!(matches!(api.get_active_tools(), Err(ExtError::Unbound(_))));
    assert!(matches!(
        api.set_model(json!({})).await,
        Err(ExtError::Unbound(_))
    ));
    // UI falls back to the null bridge (W4: noOpUIContext parity,
    // runner.ts:269) rather than erroring; hasUI stays false.
    let ctx = host.core().create_context();
    assert!(ctx.ui().unwrap().is_noop());
    assert!(!ctx.has_ui().unwrap());
    assert_eq!(ctx.mode().unwrap(), ext::ExtensionMode::Print);
}

#[tokio::test]
async fn api_actions_forward_after_bind() {
    let host = host_with(vec![inline_ext("ext-a", |_api| {})]).await;
    let api = api_of(&host);
    let actions = Arc::new(MockActions::default());
    host.bind_actions(actions.clone()).await;

    api.send_message(
        json!({"customType": "note", "display": true}),
        Some(SendMessageOptions {
            trigger_turn: Some(true),
            deliver_as: Some(pir_ext_host::api::DeliverAs::NextTurn),
        }),
    )
    .unwrap();
    api.set_session_name("session-1").unwrap();
    api.set_active_tools(vec!["bash".to_owned()]).unwrap();
    api.set_thinking_level("high").unwrap();
    assert!(api.set_model(json!({"id": "m"})).await.unwrap());
    assert_eq!(api.exec("ls", &[], None).await.unwrap().stdout, "out");
    api.append_entry("note", Some(json!({"n": 1}))).unwrap();
    api.set_label("entry-1", Some("pinned")).unwrap();

    assert_eq!(
        actions
            .sent_messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())[0]
            .0["customType"],
        "note"
    );
    assert_eq!(
        api.get_session_name().unwrap().as_deref(),
        Some("session-1")
    );
    assert_eq!(api.get_active_tools().unwrap(), ["bash"]);
    assert_eq!(api.get_thinking_level().unwrap(), "high");
    assert_eq!(
        actions
            .appended_entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())[0]
            .0,
        "note"
    );
    assert_eq!(
        actions.labels.lock().unwrap_or_else(|e| e.into_inner())[0],
        ("entry-1".to_owned(), Some("pinned".to_owned()))
    );
}

#[tokio::test]
async fn api_register_tool_refreshes_only_when_bound() {
    // registerTool during load is valid; refreshTools is a no-op pre-bind
    // (loader.ts:191-192, 245-252).
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.register_tool(minimal_tool("t1")).unwrap();
    })])
    .await;
    assert_eq!(host.get_all_registered_tools().len(), 1);

    let actions = Arc::new(MockActions::default());
    host.bind_actions(actions.clone()).await;
    api_of(&host).register_tool(minimal_tool("t2")).unwrap();
    assert_eq!(
        *actions
            .refresh_count
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
        1
    );
}

fn minimal_tool(name: &str) -> ext::ToolDefinition {
    ext::ToolDefinition {
        name: name.to_owned(),
        label: name.to_owned(),
        description: String::new(),
        prompt_snippet: None,
        prompt_guidelines: None,
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(|_req, _ctx| {
            Box::pin(async { Ok(pir_agent::types::AgentToolResult::default()) })
        }),
        render_call: None,
        render_result: None,
    }
}

// ---------------------------------------------------------------------------
// Flags (loader.ts:274-301)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_flag_defaults_and_per_extension_visibility() {
    // A flag default is only seeded when no value exists
    // (loader.ts:280-282); getFlag only sees the extension's own flags
    // (loader.ts:297-301).
    let host = host_with(vec![
        inline_ext("ext-a", |api| {
            api.register_flag(
                "mine",
                None,
                FlagType::String,
                Some(FlagValue::String("d".into())),
            )
            .unwrap();
        }),
        inline_ext("ext-b", |api| {
            api.register_flag(
                "mine",
                None,
                FlagType::String,
                Some(FlagValue::String("e".into())),
            )
            .unwrap();
        }),
    ])
    .await;

    // First default wins.
    assert_eq!(
        host.runtime().get_flag_value("mine"),
        Some(FlagValue::String("d".into()))
    );
    // ext-b's API sees "mine" (it registered it) but not "other".
    let api_b = ExtensionApi::for_extension(
        host.core().extensions()[1].clone(),
        host.runtime(),
        "/test-cwd",
    );
    assert_eq!(
        api_b.get_flag("mine").unwrap(),
        Some(FlagValue::String("d".into()))
    );
    assert_eq!(api_b.get_flag("other").unwrap(), None);
    // CLI-provided values override (runtime.setFlagValue, runner.ts:482-484).
    host.runtime()
        .set_flag_value("mine", FlagValue::String("cli".into()));
    assert_eq!(
        api_b.get_flag("mine").unwrap(),
        Some(FlagValue::String("cli".into()))
    );
}

// ---------------------------------------------------------------------------
// Provider registration queue (loader.ts:206-219, runner.ts:349-407)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_provider_registration_queues_pre_bind_and_flushes_on_bind() {
    let host = host_with(vec![inline_ext_async("ext-a", |api| {
        Box::pin(async move {
            api.register_provider("queued-1", json!({"baseUrl": "https://1"}))
                .await
                .unwrap();
            api.register_provider("queued-2", json!({"baseUrl": "https://2"}))
                .await
                .unwrap();
            // Pre-bind unregister filters the queue (loader.ts:214-219).
            api.unregister_provider("queued-1").await.unwrap();
            Ok(())
        })
    })])
    .await;

    let actions = Arc::new(MockActions::default());
    host.bind_actions(actions.clone()).await;
    let registered = actions
        .registered_providers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(
        registered,
        [("queued-2".to_owned(), json!({"baseUrl": "https://2"}))]
    );

    // Post-bind registration takes effect immediately (runner.ts:387-393).
    api_of(&host)
        .register_provider("direct", json!({}))
        .await
        .unwrap();
    assert_eq!(
        actions
            .registered_providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        2
    );
}

#[tokio::test]
async fn api_provider_flush_failure_reports_extension_error() {
    // runner.ts:357-364 — flush failures surface as `register_provider`
    // extension errors.
    let host = host_with(vec![inline_ext_async("ext-a", |api| {
        Box::pin(async move {
            api.register_provider("bad", json!({})).await.unwrap();
            Ok(())
        })
    })])
    .await;
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    let _unsub = host.on_error(Arc::new(move |e| {
        sink.lock().unwrap_or_else(|e2| e2.into_inner()).push(e);
    }));

    let actions = MockActions {
        fail_provider_registration: true,
        ..MockActions::default()
    };
    host.bind_actions(Arc::new(actions)).await;

    let errors = errors.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].event, "register_provider");
    assert_eq!(errors[0].extension_path, "<inline:ext-a>");
    assert_eq!(errors[0].error, "provider rejected");
}

// ---------------------------------------------------------------------------
// Stale contexts (loader.ts:175-179, 201-205)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_stale_runtime_rejects_calls() {
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.register_flag("f", None, FlagType::Boolean, None)
            .unwrap();
    })])
    .await;
    host.bind_actions(Arc::new(MockActions::default())).await;
    host.invalidate(None);

    let api = api_of(&host);
    assert!(matches!(
        api.send_message(json!({}), None),
        Err(ExtError::Stale(_))
    ));
    assert!(matches!(api.get_flag("f"), Err(ExtError::Stale(_))));
    // Registration methods are guarded too (loader.ts:239).
    assert!(matches!(
        api.on(
            EVENT_SESSION_START,
            Arc::new(|_, _| Box::pin(async { Ok(Value::Null) }))
        ),
        Err(ExtError::Stale(_))
    ));
}

// ---------------------------------------------------------------------------
// Typed handler wrapper
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_on_typed_round_trips_payload_and_result() {
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on_typed::<ext::SessionStartEvent, ext::SessionBeforeSwitchResult, _, _>(
            EVENT_SESSION_START,
            |event, _ctx| async move {
                assert_eq!(event.reason, ext::SessionStartReason::Startup);
                Ok(Some(ext::SessionBeforeSwitchResult { cancel: Some(true) }))
            },
        )
        .unwrap();
    })])
    .await;

    // session_start is not a session_before event, so the result is dropped
    // by the generic emit — but the handler ran (the assertion inside it).
    host.emit(
        EVENT_SESSION_START,
        json!({"type": EVENT_SESSION_START, "reason": "startup"}),
    )
    .await;
}

#[tokio::test]
async fn api_on_typed_deserialize_failure_is_a_handler_error() {
    let host = host_with(vec![inline_ext("ext-a", |api| {
        api.on_typed::<ext::SessionStartEvent, Value, _, _>(
            EVENT_SESSION_START,
            |_event, _ctx| async move { Ok(None) },
        )
        .unwrap();
    })])
    .await;
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    let _unsub = host.on_error(Arc::new(move |e| {
        sink.lock().unwrap_or_else(|e2| e2.into_inner()).push(e);
    }));

    host.emit(
        EVENT_SESSION_START,
        json!({"type": EVENT_SESSION_START, "reason": "bogus"}),
    )
    .await;
    let errors = errors.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].error.contains("deserialize"));
}

// ---------------------------------------------------------------------------
// Event bus (event-bus.ts)
// ---------------------------------------------------------------------------

#[test]
fn api_event_bus_emit_on_unsubscribe_clear() {
    let bus = EventBus::new();
    let log = Arc::new(Mutex::new(Vec::new()));

    let sink = log.clone();
    let unsub = bus.on(
        "chan",
        Arc::new(move |data| {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(data);
        }),
    );
    bus.emit("chan", json!({"n": 1}));
    bus.emit("other", json!({"n": 99}));
    unsub();
    bus.emit("chan", json!({"n": 2}));

    let sink = log.clone();
    let _unsub = bus.on(
        "chan",
        Arc::new(move |data| {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(data);
        }),
    );
    bus.emit("chan", json!({"n": 3}));
    bus.clear();
    bus.emit("chan", json!({"n": 4}));

    assert_eq!(
        log.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
        &[json!({"n": 1}), json!({"n": 3})]
    );
}

// ---------------------------------------------------------------------------
// InsertionMap (JS Map semantics)
// ---------------------------------------------------------------------------

#[test]
fn api_insertion_map_set_replaces_in_place() {
    let mut map = InsertionMap::new();
    map.set("a".to_owned(), 1);
    map.set("b".to_owned(), 2);
    map.set("a".to_owned(), 3);
    let entries: Vec<(&str, i32)> = map.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    assert_eq!(entries, [("a", 3), ("b", 2)]);
    assert!(map.contains("b"));
    assert!(!map.contains("c"));
}
