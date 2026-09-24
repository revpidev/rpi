//! M3/T27: getApiKeyAndHeaders (model-registry.ts:64-93 @ 4181f66)
//! semantics — auth_header resolution + key-omission serialization.
//!
//! Upstream anchor: model-registry.test.ts:168-179 ("unconfigured
//! compatibility auth includes static model headers") and the
//! resolveCompatibilityRequestConfig default (provider-composer.ts:554:
//! `extension?.authHeader ?? config?.authHeader ?? false`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_test_support::faux::{FauxAiProvider, FauxProvider, FauxProviderOptions};
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rpi-md-registry-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

fn slot_api(slot: &ApiSlot) -> ExtensionApi {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("factory ran")
}

struct Fixture {
    api: ExtensionApi,
    provider: Arc<FauxProvider>,
    session: rpi::core::agent_session::AgentSession,
    _tmp: TempDir,
}

/// Full pipeline: host with a capture-api extension → session → bound actions.
/// Returns the captured `ExtensionApi` for direct calls.
async fn fixture(models_json: Option<&str>) -> Fixture {
    let slot = api_slot();
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    if let Some(models_json) = models_json {
        std::fs::write(agent_dir.join("models.json"), models_json).expect("models.json");
    }

    // FauxProvider::new returns Arc<FauxProvider> already.
    let provider = FauxProvider::new(FauxProviderOptions::default());
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
        .expect("register faux provider");

    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
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
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));

    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let errors = host.load_inline(&[capture_api(slot.clone())]).await;
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");

    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
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

    rpi::core::extension_actions::bind_session_actions(&host, &created.session).await;

    let api = slot_api(&slot);
    Fixture {
        api,
        provider,
        session: created.session,
        _tmp: tmp,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// M3 (a): No auth + authHeader default false → `{ok:true, headers}` with no
/// apiKey key (model-registry.test.ts:168-179).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m3_no_auth_default_auth_header_returns_ok_headers_without_api_key() {
    let fixture = fixture(None).await;

    // Model for a provider that has no auth configured (missing-provider).
    let model = json!({
        "id": "test-model",
        "name": "Test",
        "api": "openai-completions",
        "provider": "missing-provider",
        "baseUrl": "https://example.test/v1",
        "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 1000,
        "maxTokens": 100,
        "headers": {"X-Static-Model": "static-value"},
    });

    let result = fixture
        .api
        .get_api_key_and_headers(model)
        .await
        .expect("call");
    assert_eq!(result["ok"], Value::Bool(true));
    assert!(
        result.get("apiKey").is_none() || result["apiKey"] == Value::Null,
        "apiKey key must be absent (got: {result})"
    );
    assert_eq!(
        result["headers"]["X-Static-Model"], "static-value",
        "static model headers preserved"
    );
}

/// M3 (b): No auth + authHeader true → `{ok:false, "No API key found..."}`.
/// The provider exists in models.json with authHeader:true but no apiKey and
/// no native base, so compose fails → provider not in snapshot → get_auth
/// returns Ok(None) → compat config reads authHeader:true from models.json.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m3_no_auth_with_auth_header_true_returns_error() {
    let models_json = r#"{"providers": {"compat-provider": {
        "baseUrl": "https://api.example.com/v1",
        "api": "openai-completions",
        "authHeader": true,
        "models": [{"id": "m1"}]
    }}}"#;

    let fixture = fixture(Some(models_json)).await;

    // The model doesn't need to be in the runtime snapshot — getApiKeyAndHeaders
    // deserializes the model JSON directly and calls get_auth, which returns
    // Ok(None) for a provider not in the resolved set.
    let model = json!({
        "id": "m1",
        "name": "M1",
        "api": "openai-completions",
        "provider": "compat-provider",
        "baseUrl": "https://api.example.com/v1",
        "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128000,
        "maxTokens": 16384,
    });

    let result = fixture
        .api
        .get_api_key_and_headers(model)
        .await
        .expect("call");
    assert_eq!(result["ok"], Value::Bool(false));
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("No API key found"),
        "error message: {result}"
    );
}

/// T27 (c): Success branch omits None-valued keys. A provider with no auth
/// resolved and no model headers → `{ok:true}` with no apiKey/headers/baseUrl.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t27_ok_none_branch_omits_empty_keys() {
    let fixture = fixture(None).await;

    let model = json!({
        "id": "test-model",
        "name": "Test",
        "api": "openai-completions",
        "provider": "missing-provider",
        "baseUrl": "https://example.test/v1",
        "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 1000,
        "maxTokens": 100,
    });

    let result = fixture
        .api
        .get_api_key_and_headers(model)
        .await
        .expect("call");
    // No auth resolved + authHeader defaults false → {ok:true}. No model
    // headers → headers key omitted.
    assert_eq!(result["ok"], Value::Bool(true));
    assert!(
        result.get("apiKey").is_none() || result["apiKey"] == Value::Null,
        "apiKey must be absent (got: {result})"
    );
    assert!(
        result.get("headers").is_none() || result["headers"] == Value::Null,
        "headers must be absent when empty (got: {result})"
    );
    assert!(
        result.get("baseUrl").is_none() || result["baseUrl"] == Value::Null,
        "baseUrl must be absent"
    );
}

// ---------------------------------------------------------------------------
// #8964 (1f78cea7a): ctx.modelRegistry.stream / streamSimple — extension
// model calls through configured providers with resolved authentication.
// Upstream anchor: test/suite/regressions/8964-extension-provider-streaming.test.ts
// (adapted: the rpi carrier collects the stream server-side — both methods
// drain the same event vocabulary + final result).
// ---------------------------------------------------------------------------

/// Both entries stream through the faux provider: text deltas accumulate
/// into the collected events and the final message lands in `result`
/// (upstream asserts `streamedText` + `result` content/stopReason).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_and_stream_simple_collect_events_and_result() {
    use futures::StreamExt;

    for simple in [false, true] {
        let fixture = fixture(None).await;
        fixture
            .provider
            .set_responses(vec![rpi_test_support::faux::faux_assistant_message(
                "custom provider response",
                rpi_test_support::faux::FauxAssistantOptions::default(),
            )
            .into()]);
        let model = serde_json::to_value(fixture.provider.get_model(None).expect("faux model"))
            .expect("model json");
        let context = json!({"messages": [
            {"role": "user", "content": "Hello", "timestamp": 0}
        ]});

        let stream = if simple {
            fixture
                .api
                .model_registry_stream_simple(model, context, None)
                .expect("streamSimple")
        } else {
            fixture
                .api
                .model_registry_stream(model, context, None)
                .expect("stream")
        };
        let events: Vec<rpi_ai::types::StreamEvent> = stream.clone().collect().await;
        let result = stream.result().await.expect("result resolves");

        // Event vocabulary: start before updates, terminal done.
        assert_eq!(events.first().map(|e| kind_of(e)), Some("start"));
        assert_eq!(events.last().map(|e| kind_of(e)), Some("done"));
        let streamed_text: String = events
            .iter()
            .filter_map(|event| match event {
                rpi_ai::types::StreamEvent::TextDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed_text, "custom provider response");
        assert_eq!(result.stop_reason, rpi_ai::types::StopReason::Stop);
        let text = result
            .content
            .iter()
            .filter_map(|block| match block {
                rpi_ai::types::AssistantContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "custom provider response");
    }
}

fn kind_of(event: &rpi_ai::types::StreamEvent) -> &'static str {
    use rpi_ai::types::StreamEvent;
    match event {
        StreamEvent::Start { .. } => "start",
        StreamEvent::TextStart { .. } => "text_start",
        StreamEvent::TextDelta { .. } => "text_delta",
        StreamEvent::TextEnd { .. } => "text_end",
        StreamEvent::ThinkingStart { .. } => "thinking_start",
        StreamEvent::ThinkingDelta { .. } => "thinking_delta",
        StreamEvent::ThinkingEnd { .. } => "thinking_end",
        StreamEvent::ToolCallStart { .. } => "toolcall_start",
        StreamEvent::ToolCallDelta { .. } => "toolcall_delta",
        StreamEvent::ToolCallEnd { .. } => "toolcall_end",
        StreamEvent::Done { .. } => "done",
        StreamEvent::Error { .. } => "error",
    }
}

/// `streamSimple` maps the provider-neutral `reasoning` option into the
/// request (upstream docs: "provider-neutral options such as reasoning").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_simple_options_reach_the_provider() {
    use futures::StreamExt;

    let fixture = fixture(None).await;
    fixture
        .provider
        .set_responses(vec![rpi_test_support::faux::faux_assistant_message(
            "ok",
            rpi_test_support::faux::FauxAssistantOptions::default(),
        )
        .into()]);
    let model = serde_json::to_value(fixture.provider.get_model(None).expect("faux model"))
        .expect("model json");
    let context = json!({"messages": [
        {"role": "user", "content": "Hello", "timestamp": 0}
    ]});

    // Invalid reasoning level for the simple shape ("off" is not a
    // ThinkingLevel) → setup error surfaces as an Err at the API layer.
    let error = match fixture.api.model_registry_stream_simple(
        model.clone(),
        context.clone(),
        Some(json!({"reasoning": "off"})),
    ) {
        Err(error) => error,
        Ok(_) => panic!("off is not a ThinkingLevel; expected an error"),
    };
    assert!(error.to_string().contains("reasoning"), "{error}");

    // Valid level parses and the request reaches the provider.
    let stream = fixture
        .api
        .model_registry_stream_simple(model, context, Some(json!({"reasoning": "high"})))
        .expect("streamSimple with reasoning");
    let events: Vec<rpi_ai::types::StreamEvent> = stream.collect().await;
    assert!(
        events.iter().any(|event| kind_of(event) == "done"),
        "terminal done event present"
    );
}

/// A dead session (weak upgrade fails) answers ONE error event plus an
/// error result — the collected shape of upstream "setup failures produce
/// error events and error results".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_after_session_drop_collects_error_event() {
    use futures::StreamExt;

    let fixture = fixture(None).await;
    let model = serde_json::to_value(fixture.provider.get_model(None).expect("faux model"))
        .expect("model json");
    drop(fixture.session);
    // Let the weak handle observe the drop.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let stream = fixture
        .api
        .model_registry_stream(model, json!({"messages": []}), None)
        .expect("api call succeeds (bound actions remain)");
    let events: Vec<rpi_ai::types::StreamEvent> = stream.clone().collect().await;
    let result = stream.result().await;
    assert_eq!(events.len(), 1, "single terminal error event: {events:?}");
    assert!(matches!(
        events.first(),
        Some(rpi_ai::types::StreamEvent::Error { .. })
    ));
    let result = result.expect("error result present");
    assert_eq!(result.stop_reason, rpi_ai::types::StopReason::Error);
    assert!(result.error_message.is_some());
}
