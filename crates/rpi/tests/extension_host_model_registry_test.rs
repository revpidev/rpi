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
    _session: rpi::core::agent_session::AgentSession,
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
        .register_native_provider(Arc::new(FauxAiProvider::new(provider)))
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
        _session: created.session,
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
