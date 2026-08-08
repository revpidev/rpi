//! Canonical model/auth runtime (design §6.1 services).
//!
//! Port of the T10-reachable subset of
//! `packages/coding-agent/src/core/model-runtime.ts` @ pi 0.82.1 (2efa728)
//! plus `runtime-credentials.ts`.
//!
//! T10 subset boundaries (full parity lands with T13 providers):
//! - No built-in provider catalog: pir-ai implements the API adapters but the
//!   38 provider factories are T13. The catalog consists of models.json
//!   custom providers plus SDK/extension registrations.
//! - models.json `apiKey` is treated as an env var name
//!   ([`env_api_key_auth`]); command/raw-key config values
//!   (resolve-config-value.ts) and `oauth: "radius"` are T13.
//! - Per-model `api` overrides inside one provider are not honored (the
//!   provider-level `api` streams all models).
//! - Availability is recomputed on each `get_available` call (upstream
//!   coalesces concurrent refreshes onto one in-flight promise — the
//!   single-threaded headless flows cannot observe the difference).
//!
//! W6-C notes (remote catalog overlay, model-runtime.ts:133-172, 516-537):
//! - `ModelsStore` persistence (file next to models.json, upstream
//!   `FileModelsStore`) and `Models::refresh` network plumbing landed:
//!   `CreateModelRuntimeOptions` gains `modelsStore` / `modelsStorePath` /
//!   `allowModelNetwork` / `modelRefreshTimeoutMs`; `refresh(options)` runs
//!   dynamic catalog refreshes with `allowNetwork` defaulting to
//!   `modelNetworkEnabled` (= `PIR_OFFLINE` unset).
//! - Built-in providers are not registered yet, so the
//!   [`remote_catalog_provider`] decorator has no runtime consumer in this
//!   wave; the registration wave wraps them like upstream
//!   (model-runtime.ts:144-150).
//! - A corrupt `models-store.json` falls back to an in-memory store with a
//!   warning (upstream surfaces per-read `JSON.parse` errors into the
//!   refresh result; see D-036).
//!
//! Upstream mutates through plain class fields; here the mutable registries
//! live behind mutexes so all methods take `&self` (JS has no borrow
//! discipline — this is structural, not behavioral).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use pir_ai::api::anthropic_messages::AnthropicMessages;
use pir_ai::api::openai_completions::OpenAiCompletions;
use pir_ai::api::openai_responses::OpenAiResponses;
use pir_ai::auth::file_store::FileCredentialStore;
use pir_ai::auth::helpers::env_api_key_auth;
use pir_ai::auth::interaction::AuthInteraction;
use pir_ai::auth::resolve::{
    resolve_provider_auth, AuthResolutionOverrides, ModelsError, ModelsErrorCode,
};
use pir_ai::auth::types::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthResult, AuthType, Credential,
    CredentialInfo, CredentialStore, CredentialType, DefaultAuthContext, ModifyFn, ProviderAuth,
};
use pir_ai::models::{
    create_provider, CreateModelsOptions, CreateProviderOptions, Models, ModelsRefreshOptions,
    ModelsRefreshResult, ModelsSimpleStreamOptions, ModelsStreamOptions, Provider, ProviderApi,
    ProviderStreams, RefreshModelsContext,
};
use pir_ai::models_json::{
    ModelConfig, ModelsJsonModel, ModelsJsonModelOverride, ModelsJsonProvider, OrderedMap,
};
use pir_ai::models_store::{InMemoryModelsStore, JsonFileModelsStore, ModelsStore};
use pir_ai::types::{
    ApiKind, AssistantMessage, Context, Model, ModelCompat, ModelCost, ModelCostRates, ProviderEnv,
    ProviderHeaders, SimpleStreamOptions, StreamOptions,
};
use pir_ai::utils::event_stream::AssistantMessageEventStream;

use crate::config::{get_agent_dir, ENV_OFFLINE};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

// ============================================================================
// RuntimeCredentials (runtime-credentials.ts)
// ============================================================================

/// Async credential store overlay for non-persistent runtime API keys
/// (`RuntimeCredentials`, runtime-credentials.ts:4-48).
pub struct RuntimeCredentials {
    store: Arc<dyn CredentialStore>,
    overrides: RwLock<HashMap<String, String>>,
}

impl RuntimeCredentials {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        RuntimeCredentials {
            store,
            overrides: RwLock::new(HashMap::new()),
        }
    }

    pub fn set_runtime_api_key(&self, provider_id: &str, api_key: &str) {
        write(&self.overrides).insert(provider_id.to_owned(), api_key.to_owned());
    }

    pub fn remove_runtime_api_key(&self, provider_id: &str) {
        write(&self.overrides).remove(provider_id);
    }

    pub fn has_runtime_api_key(&self, provider_id: &str) -> bool {
        read(&self.overrides).contains_key(provider_id)
    }

    fn runtime_override(&self, provider_id: &str) -> Option<String> {
        read(&self.overrides).get(provider_id).cloned()
    }

    fn override_provider_ids(&self) -> Vec<String> {
        read(&self.overrides).keys().cloned().collect()
    }
}

#[async_trait::async_trait]
impl CredentialStore for RuntimeCredentials {
    async fn read(&self, provider_id: &str) -> Result<Option<Credential>, ModelsError> {
        if let Some(key) = self.runtime_override(provider_id) {
            return Ok(Some(Credential::ApiKey(ApiKeyCredential {
                key: Some(key),
                env: None,
            })));
        }
        self.store.read(provider_id).await
    }

    async fn list(&self) -> Result<Vec<CredentialInfo>, ModelsError> {
        let mut entries: Vec<CredentialInfo> = self.store.list().await?;
        for provider_id in self.override_provider_ids() {
            if !entries.iter().any(|entry| entry.provider_id == provider_id) {
                entries.push(CredentialInfo {
                    provider_id,
                    credential_type: CredentialType::ApiKey,
                });
            }
        }
        Ok(entries)
    }

    async fn modify(
        &self,
        provider_id: &str,
        f: ModifyFn,
    ) -> Result<Option<Credential>, ModelsError> {
        self.store.modify(provider_id, f).await
    }

    async fn delete(&self, provider_id: &str) -> Result<(), ModelsError> {
        self.remove_runtime_api_key(provider_id);
        self.store.delete(provider_id).await
    }
}

// ============================================================================
// Extension provider registration input (provider-composer.ts subset)
// ============================================================================

/// `ProviderConfigInput["models"][number]` (provider-composer.ts:56-68).
/// `Deserialize` (camelCase) serves the extension host's
/// `registerProvider(name, config)` JSON boundary (T15 W3).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderConfigModel {
    pub id: String,
    pub name: Option<String>,
    pub api: Option<String>,
    pub base_url: Option<String>,
    pub reasoning: bool,
    pub thinking_level_map: Option<pir_ai::types::ThinkingLevelMap>,
    pub input: Vec<pir_ai::types::InputModality>,
    pub cost: Option<pir_ai::types::ModelCost>,
    pub context_window: u32,
    pub max_tokens: u32,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<pir_ai::types::ModelCompat>,
}

/// `ProviderConfigInput` (provider-composer.ts:44-69) — T10 subset: the
/// closure-bearing fields (`streamSimple`, `oauth`, `refreshModels`) are
/// rejected by the extension action layer (candidate deviation, T15 W3).
/// `Deserialize` (camelCase) serves the host's JSON boundary.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderConfigInput {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    pub auth_header: Option<bool>,
    pub models: Option<Vec<ProviderConfigModel>>,
}

// ============================================================================
// Snapshot
// ============================================================================

/// `ModelRuntimeSnapshot` (model-runtime.ts:50-56).
#[derive(Debug, Default)]
struct ModelRuntimeSnapshot {
    all: Vec<Model>,
    available: Vec<Model>,
    configured_providers: HashSet<String>,
    stored_providers: HashSet<String>,
    auth: HashMap<String, AuthCheck>,
}

// ============================================================================
// Api registry (T03 adapters; requirements §5.1 remainder is T13)
// ============================================================================

/// `getApiProvider` equivalent for the adapters pir-ai implements.
fn api_streams(api: &str) -> Option<Arc<dyn ProviderStreams>> {
    match api {
        ApiKind::ANTHROPIC_MESSAGES => Some(Arc::new(AnthropicMessages)),
        ApiKind::OPENAI_COMPLETIONS => Some(Arc::new(OpenAiCompletions)),
        ApiKind::OPENAI_RESPONSES | ApiKind::AZURE_OPENAI_RESPONSES => {
            Some(Arc::new(OpenAiResponses))
        }
        _ => None,
    }
}

// ============================================================================
// Options
// ============================================================================

/// `modelsPath` tri-state (model-runtime.ts:135-136): `null` upstream
/// disables the file entirely.
#[derive(Debug, Clone, Default)]
pub enum ModelsPathInput {
    /// `{agentDir}/models.json`.
    #[default]
    Default,
    Path(PathBuf),
    /// Upstream `modelsPath: null` — no models.json.
    Disabled,
}

/// Create-time network model refresh timeout (model-runtime.ts:164,
/// package-manager-cli.ts:404): 15 seconds.
pub const DEFAULT_MODEL_REFRESH_TIMEOUT_MS: u64 = 15_000;

/// `CreateModelRuntimeOptions` (model-runtime.ts:58-70), T10 subset. W6-C
/// added the network refresh options (see module docs).
#[derive(Default)]
pub struct CreateModelRuntimeOptions {
    /// Credential storage. Default: file at `auth_path`.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    /// Default: `{agentDir}/auth.json`.
    pub auth_path: Option<PathBuf>,
    pub models_path: ModelsPathInput,
    /// Persistent model storage for dynamic provider catalogs. Default: file
    /// at `modelsStorePath` (or `{models.json dir}/models-store.json`) when
    /// models.json is enabled, else in-memory (model-runtime.ts:138-142).
    pub models_store: Option<Arc<dyn ModelsStore>>,
    /// File store path used when `models_store` is unset.
    pub models_store_path: Option<PathBuf>,
    /// Allow `create()` to refresh model catalogs over the network. Defaults
    /// to false (model-runtime.ts:66).
    pub allow_model_network: bool,
    /// Timeout for the create-time network model refresh (ms). Default:
    /// 15_000 (model-runtime.ts:67-68).
    pub model_refresh_timeout_ms: Option<u64>,
}

/// `ModelRuntimeAuthOverrides` (model-runtime.ts:72-75).
#[derive(Debug, Clone, Default)]
pub struct ModelRuntimeAuthOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
}

// ============================================================================
// ModelRuntime
// ============================================================================

/// Configured pi-ai Models collection used by coding-agent and SDK consumers
/// (`ModelRuntime`, model-runtime.ts:94-593).
pub struct ModelRuntime {
    models: Models,
    credentials: Arc<RuntimeCredentials>,
    models_path: Option<PathBuf>,
    config: Mutex<ModelConfig>,
    /// Whether network model-catalog refreshes are allowed at all
    /// (`PI_OFFLINE` unset, model-runtime.ts:157).
    model_network_enabled: bool,
    /// Insertion-ordered like the upstream JS `Map`s: provider enumeration
    /// order is observable (initial-model fallback, available listings).
    native_providers: Mutex<OrderedMap<Arc<dyn Provider>>>,
    extension_providers: Mutex<OrderedMap<ProviderConfigInput>>,
    composition_errors: Mutex<OrderedMap<String>>,
    availability_error: Mutex<Option<String>>,
    snapshot: RwLock<ModelRuntimeSnapshot>,
}

/// `mergeHeaders` (model-runtime.ts:77-91): case-insensitive replace, new
/// key keeps its original casing.
fn merge_headers(
    base: Option<&ProviderHeaders>,
    override_: Option<&ProviderHeaders>,
) -> Option<ProviderHeaders> {
    if base.is_none() && override_.is_none() {
        return None;
    }
    let mut merged: ProviderHeaders = base.cloned().unwrap_or_default();
    for (name, value) in override_.into_iter().flatten() {
        let lower = name.to_lowercase();
        let existing: Vec<String> = merged
            .keys()
            .filter(|k| k.to_lowercase() == lower)
            .cloned()
            .collect();
        for key in existing {
            merged.remove(&key);
        }
        merged.insert(name.clone(), value.clone());
    }
    Some(merged)
}

fn strings_to_headers<'a>(
    headers: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> ProviderHeaders {
    headers
        .into_iter()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect()
}

/// `composeApiKeyAuth.resolve` wrapper (provider-composer.ts:333-356 +
/// `withConfiguredAuth`:250-262): merges the configured provider headers into
/// every resolved auth result and, when `authHeader` is set, adds
/// `Authorization: Bearer <key>`.
struct ConfiguredApiKeyAuth {
    inner: Arc<dyn ApiKeyAuth>,
    headers: Option<ProviderHeaders>,
    auth_header: bool,
}

#[async_trait::async_trait]
impl ApiKeyAuth for ConfiguredApiKeyAuth {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn supports_login(&self) -> bool {
        self.inner.supports_login()
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<ApiKeyCredential, ModelsError> {
        self.inner.login(interaction).await
    }

    async fn check(
        &self,
        ctx: &dyn AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthCheck>, ModelsError> {
        self.inner.check(ctx, credential).await
    }

    async fn resolve(
        &self,
        ctx: &dyn AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        let Some(mut result) = self.inner.resolve(ctx, credential).await? else {
            return Ok(None);
        };
        result.auth.headers = merge_headers(result.auth.headers.as_ref(), self.headers.as_ref());
        if self.auth_header {
            let Some(api_key) = result.auth.api_key.clone() else {
                return Err(ModelsError::new(
                    ModelsErrorCode::Auth,
                    "authHeader requires a resolved API key",
                ));
            };
            let authorization: ProviderHeaders = [(
                "Authorization".to_owned(),
                Some(format!("Bearer {api_key}")),
            )]
            .into_iter()
            .collect();
            result.auth.headers = merge_headers(result.auth.headers.as_ref(), Some(&authorization));
        }
        Ok(Some(result))
    }
}

/// No-op passthrough when there is nothing to configure.
fn wrap_api_key_auth(
    inner: Arc<dyn ApiKeyAuth>,
    headers: &Option<ProviderHeaders>,
    auth_header: bool,
) -> Arc<dyn ApiKeyAuth> {
    if headers.is_none() && !auth_header {
        return inner;
    }
    Arc::new(ConfiguredApiKeyAuth {
        inner,
        headers: headers.clone(),
        auth_header,
    })
}

fn wrap_provider_auth(
    mut auth: ProviderAuth,
    headers: &Option<ProviderHeaders>,
    auth_header: bool,
) -> ProviderAuth {
    if let Some(api_key) = auth.api_key.take() {
        auth.api_key = Some(wrap_api_key_auth(api_key, headers, auth_header));
    }
    auth
}

/// Base passthrough with auth resolution overridden by configured
/// headers/`authHeader` (models.json/extension overlay without its own `api`).
struct AuthOverridingProvider {
    base: Arc<dyn Provider>,
    auth: ProviderAuth,
}

impl Provider for AuthOverridingProvider {
    fn id(&self) -> &str {
        self.base.id()
    }

    fn name(&self) -> &str {
        self.base.name()
    }

    fn base_url(&self) -> Option<&str> {
        self.base.base_url()
    }

    fn headers(&self) -> Option<&ProviderHeaders> {
        self.base.headers()
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.base.get_models()
    }

    /// `filterModels` forwards to the base (provider-composer.ts:493-494).
    fn filter_models(&self, models: Vec<Model>, credential: Option<&Credential>) -> Vec<Model> {
        self.base.filter_models(models, credential)
    }

    /// Overlays keep the base's dynamic catalog refresh
    /// (provider-composer.ts:475-478).
    fn refresh_models(
        &self,
        context: RefreshModelsContext,
    ) -> Option<BoxFuture<'_, Result<(), ModelsError>>> {
        self.base.refresh_models(context)
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.base.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.base.stream_simple(model, context, options)
    }
}

/// Composed provider delegating `refreshModels` to its base
/// (provider-composer.ts:475-478 `refreshModels: base?.refreshModels …`), so
/// an overlay built by [`ModelRuntime::compose_provider`] keeps the base's
/// dynamic catalog refresh.
struct RefreshDelegatingProvider {
    base: Arc<dyn Provider>,
    inner: Arc<dyn Provider>,
}

impl Provider for RefreshDelegatingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn base_url(&self) -> Option<&str> {
        self.inner.base_url()
    }

    fn headers(&self) -> Option<&ProviderHeaders> {
        self.inner.headers()
    }

    fn auth(&self) -> &ProviderAuth {
        self.inner.auth()
    }

    fn get_models(&self) -> Vec<Model> {
        self.inner.get_models()
    }

    /// `filterModels` comes from the native base provider only
    /// (provider-composer.ts:492-494).
    fn filter_models(&self, models: Vec<Model>, credential: Option<&Credential>) -> Vec<Model> {
        self.base.filter_models(models, credential)
    }

    fn refresh_models(
        &self,
        context: RefreshModelsContext,
    ) -> Option<BoxFuture<'_, Result<(), ModelsError>>> {
        self.base.refresh_models(context)
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.inner.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.inner.stream_simple(model, context, options)
    }
}

/// `mergeCompat` (provider-composer.ts:78-98): shallow field override, with
/// the three nested objects merged key-wise. Implemented over the serialized
/// form so the field list tracks `ModelCompat` automatically.
fn merge_compat(base: Option<&ModelCompat>, override_: &ModelCompat) -> ModelCompat {
    let base_value = base.and_then(|base| serde_json::to_value(base).ok());
    let override_value = serde_json::to_value(override_).ok();
    let (Some(base_value), Some(override_value)) = (base_value, override_value) else {
        return override_.clone();
    };
    let (Some(base_map), Some(override_map)) = (base_value.as_object(), override_value.as_object())
    else {
        return override_.clone();
    };
    let mut merged = base_map.clone();
    for (key, value) in override_map {
        merged.insert(key.clone(), value.clone());
    }
    for key in [
        "openRouterRouting",
        "vercelGatewayRouting",
        "chatTemplateKwargs",
    ] {
        let base_nested = base_map.get(key).and_then(serde_json::Value::as_object);
        let override_nested = override_map.get(key).and_then(serde_json::Value::as_object);
        if base_nested.is_some() || override_nested.is_some() {
            let mut nested = base_nested.cloned().unwrap_or_default();
            if let Some(override_nested) = override_nested {
                for (key, value) in override_nested {
                    nested.insert(key.clone(), value.clone());
                }
            }
            merged.insert(key.to_owned(), serde_json::Value::Object(nested));
        }
    }
    serde_json::from_value(serde_json::Value::Object(merged)).unwrap_or_else(|_| override_.clone())
}

/// `applyModelOverride` (provider-composer.ts:100-122) plus the
/// `modelOverrides` entry of `rawModelHeaders` (provider-composer.ts:384-397,
/// merged *under* definition/extension headers).
fn apply_model_override(mut model: Model, override_: &ModelsJsonModelOverride) -> Model {
    if let Some(name) = &override_.name {
        model.name = name.clone();
    }
    if let Some(reasoning) = override_.reasoning {
        model.reasoning = reasoning;
    }
    if let Some(thinking_level_map) = &override_.thinking_level_map {
        model.thinking_level_map = Some(match model.thinking_level_map.take() {
            Some(mut base) => {
                base.extend(thinking_level_map.clone());
                base
            }
            None => thinking_level_map.clone(),
        });
    }
    if let Some(input) = &override_.input {
        model.input = input.clone();
    }
    if let Some(cost) = &override_.cost {
        model.cost = ModelCost {
            rates: ModelCostRates {
                input: cost.input.unwrap_or(model.cost.rates.input),
                output: cost.output.unwrap_or(model.cost.rates.output),
                cache_read: cost.cache_read.unwrap_or(model.cost.rates.cache_read),
                cache_write: cost.cache_write.unwrap_or(model.cost.rates.cache_write),
            },
            tiers: cost.tiers.clone().or_else(|| model.cost.tiers.take()),
        };
    }
    if let Some(context_window) = override_.context_window {
        model.context_window = context_window as u32;
    }
    if let Some(max_tokens) = override_.max_tokens {
        model.max_tokens = max_tokens as u32;
    }
    if let Some(compat) = &override_.compat {
        model.compat = Some(merge_compat(model.compat.as_ref(), compat));
    }
    if let Some(override_headers) = &override_.headers {
        let mut headers: BTreeMap<String, String> = override_headers.clone().into_iter().collect();
        if let Some(existing) = model.headers.take() {
            headers.extend(existing);
        }
        model.headers = Some(headers);
    }
    model
}

impl ModelRuntime {
    /// `ModelRuntime.create` (model-runtime.ts:133-172).
    pub async fn create(options: CreateModelRuntimeOptions) -> Arc<Self> {
        let auth_path = options
            .auth_path
            .unwrap_or_else(|| get_agent_dir().join("auth.json"));
        let store: Arc<dyn CredentialStore> = options
            .credentials
            .unwrap_or_else(|| Arc::new(FileCredentialStore::new(auth_path)));
        let credentials = Arc::new(RuntimeCredentials::new(store));
        let models_path = match options.models_path {
            ModelsPathInput::Default => Some(get_agent_dir().join("models.json")),
            ModelsPathInput::Path(path) => Some(path),
            ModelsPathInput::Disabled => None,
        };
        let config = ModelConfig::load(models_path.as_deref()).await;
        // modelsStore (model-runtime.ts:138-142): the file next to
        // models.json, or in-memory when models.json is disabled. A corrupt
        // store file falls back to in-memory (D-036).
        let models_store: Arc<dyn ModelsStore> = match options.models_store {
            Some(store) => store,
            None => match &models_path {
                Some(models_path) => {
                    let store_path = options.models_store_path.unwrap_or_else(|| {
                        models_path
                            .parent()
                            .map(|parent| parent.join("models-store.json"))
                            .unwrap_or_else(|| models_path.with_file_name("models-store.json"))
                    });
                    match JsonFileModelsStore::load(store_path).await {
                        Ok(store) => Arc::new(store),
                        Err(error) => {
                            tracing::warn!(
                                "models store load failed (falling back to in-memory): {error}"
                            );
                            Arc::new(InMemoryModelsStore::new())
                        }
                    }
                }
                None => Arc::new(InMemoryModelsStore::new()),
            },
        };
        let model_network_enabled = std::env::var_os(ENV_OFFLINE).is_none();
        let models = Models::new(Some(CreateModelsOptions {
            credentials: Some(credentials.clone()),
            auth_context: None,
            models_store: Some(models_store),
        }));

        let runtime = Arc::new(ModelRuntime {
            models,
            credentials,
            models_path,
            config: Mutex::new(config),
            model_network_enabled,
            native_providers: Mutex::new(OrderedMap::default()),
            extension_providers: Mutex::new(OrderedMap::default()),
            composition_errors: Mutex::new(OrderedMap::default()),
            availability_error: Mutex::new(None),
            snapshot: RwLock::new(ModelRuntimeSnapshot::default()),
        });
        runtime.rebuild_providers();
        // create() refreshes dynamic catalogs over the network only when
        // explicitly enabled, with a default 15s timeout (model-runtime.ts:
        // 161-170).
        let refresh_from_network = model_network_enabled && options.allow_model_network;
        let token = refresh_from_network.then(CancellationToken::new);
        if let Some(token) = &token {
            let abort = token.clone();
            let timeout_ms = options
                .model_refresh_timeout_ms
                .unwrap_or(DEFAULT_MODEL_REFRESH_TIMEOUT_MS);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
                abort.cancel();
            });
        }
        runtime
            .refresh(Some(ModelsRefreshOptions {
                allow_network: Some(refresh_from_network),
                force: None,
                signal: token,
            }))
            .await;
        // The refresh result (errors/abort) is intentionally not surfaced:
        // refreshed models remain usable and availability errors are recorded
        // in `get_error` (upstream awaits without inspecting, model-runtime.ts:
        // 167).
        runtime
    }

    fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        let mut push = |id: &str| {
            if !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_owned());
            }
        };
        for id in lock(&self.native_providers).keys() {
            push(id);
        }
        for id in lock(&self.config).provider_ids() {
            push(id);
        }
        for id in lock(&self.extension_providers).keys() {
            push(id);
        }
        ids
    }

    /// `composeModelProvider` T10 subset: models.json config + extension
    /// registration over an optional native base (provider-composer.ts:412).
    fn compose_provider(
        provider_id: &str,
        base: Option<&Arc<dyn Provider>>,
        config: Option<&ModelsJsonProvider>,
        extension: Option<&ProviderConfigInput>,
    ) -> Result<Arc<dyn Provider>, String> {
        let name = extension
            .and_then(|e| e.name.clone())
            .or_else(|| config.and_then(|c| c.name.clone()))
            .or_else(|| base.map(|b| b.name().to_owned()))
            .unwrap_or_else(|| provider_id.to_owned());
        let base_url = extension
            .and_then(|e| e.base_url.clone())
            .or_else(|| config.and_then(|c| c.base_url.clone()))
            .or_else(|| base.and_then(|b| b.base_url().map(str::to_owned)));

        let api_key_value = extension
            .and_then(|e| e.api_key.clone())
            .or_else(|| config.and_then(|c| c.api_key.clone()));

        // `configuredHeaders` + `authHeader` (provider-composer.ts:271-277,
        // 305): they wrap auth *resolution* so every request path — including
        // `stream`/`stream_simple` — sees them.
        let configured_headers = merge_headers(
            config
                .and_then(|c| c.headers.as_ref())
                .map(|headers| strings_to_headers(headers.iter()))
                .as_ref(),
            extension
                .and_then(|e| e.headers.as_ref())
                .map(|headers| strings_to_headers(headers.iter()))
                .as_ref(),
        );
        let auth_header = extension
            .and_then(|e| e.auth_header)
            .or_else(|| config.and_then(|c| c.auth_header))
            .unwrap_or(false);

        let auth = match &api_key_value {
            Some(env_var) => ProviderAuth {
                api_key: Some(wrap_api_key_auth(
                    Arc::new(env_api_key_auth(
                        format!("{provider_id} API key"),
                        &[env_var.as_str()],
                    )),
                    &configured_headers,
                    auth_header,
                )),
                oauth: None,
            },
            None => {
                let base_auth = base.map(|b| b.auth().clone()).ok_or_else(|| {
                    format!("Provider {provider_id}: no authentication method configured.")
                })?;
                wrap_provider_auth(base_auth, &configured_headers, auth_header)
            }
        };

        let api = extension
            .and_then(|e| e.api.clone())
            .or_else(|| config.and_then(|c| c.api.clone()));

        // Models are built (and validated) before stream dispatch so a
        // model-level problem surfaces the upstream per-model composition
        // error (provider-composer.ts validates `getModels()` first).
        // The base-passthrough branch skips construction entirely.
        let mut models = match (&api, base) {
            (None, Some(_)) => Vec::new(),
            _ => match extension.and_then(|e| e.models.clone()) {
                Some(models) => models
                    .into_iter()
                    .map(|m| config_model_to_model(provider_id, &api, base_url.as_deref(), m))
                    .collect::<Result<Vec<_>, _>>()?,
                None => match config.and_then(|c| c.models.clone()) {
                    Some(models) => models
                        .into_iter()
                        .map(|m| {
                            json_model_to_model(
                                provider_id,
                                &api,
                                base_url.as_deref(),
                                config.and_then(|c| c.compat.as_ref()),
                                m,
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    None => base.map(|b| b.get_models()).unwrap_or_default(),
                },
            },
        };
        // models.json `modelOverrides` are the topmost user-config layer:
        // applied once, after model construction (provider-composer.ts:434-437).
        if let Some(overrides) = config.and_then(|c| c.model_overrides.as_ref()) {
            models = models
                .into_iter()
                .map(|model| match overrides.get(&model.id) {
                    Some(override_) => apply_model_override(model, override_),
                    None => model,
                })
                .collect();
        }

        let streams = match (&api, base) {
            (Some(api), _) => api_streams(api)
                .ok_or_else(|| format!("No API provider registered for api: {api}"))?,
            (None, Some(base)) => {
                // Overlay without its own api: stream through the base
                // provider; configured headers/authHeader still wrap auth
                // resolution (`composeApiKeyAuth`, provider-composer.ts:293).
                if configured_headers.is_none() && !auth_header {
                    return Ok(base.clone());
                }
                return Ok(Arc::new(AuthOverridingProvider {
                    base: base.clone(),
                    auth: wrap_provider_auth(base.auth().clone(), &configured_headers, auth_header),
                }));
            }
            (None, None) => {
                return Err(format!("Provider {provider_id}: no api configured."));
            }
        };

        let headers = base.and_then(|b| b.headers().cloned());

        let composed = create_provider(CreateProviderOptions {
            id: provider_id.to_owned(),
            name: Some(name),
            base_url,
            headers,
            auth,
            models,
            api: ProviderApi::Single(streams),
        });
        // Overlays keep the base's dynamic catalog refresh
        // (provider-composer.ts:475-478).
        Ok(match base {
            Some(base) => Arc::new(RefreshDelegatingProvider {
                base: base.clone(),
                inner: composed,
            }),
            None => composed,
        })
    }

    fn recompose_provider(&self, provider_id: &str) {
        let base = lock(&self.native_providers).get(provider_id).cloned();
        let config = lock(&self.config).get_provider(provider_id).cloned();
        let extension = lock(&self.extension_providers).get(provider_id).cloned();

        if base.is_none() && config.is_none() && extension.is_none() {
            self.models.delete_provider(provider_id);
            lock(&self.composition_errors).remove(provider_id);
            return;
        }
        if base.is_some() && config.is_none() && extension.is_none() {
            // No overlays: use the native provider untouched
            // (model-runtime.ts:208-212).
            if let Some(base) = base {
                self.models.set_provider(base);
            }
            lock(&self.composition_errors).remove(provider_id);
            return;
        }
        match Self::compose_provider(
            provider_id,
            base.as_ref(),
            config.as_ref(),
            extension.as_ref(),
        ) {
            Ok(provider) => {
                self.models.set_provider(provider);
                lock(&self.composition_errors).remove(provider_id);
            }
            Err(error) => {
                lock(&self.composition_errors).insert(provider_id.to_owned(), error);
                match base {
                    Some(base) => self.models.set_provider(base),
                    None => self.models.delete_provider(provider_id),
                }
            }
        }
    }

    fn rebuild_providers(&self) {
        self.models.clear_providers();
        lock(&self.composition_errors).clear();
        for provider_id in self.provider_ids() {
            self.recompose_provider(&provider_id);
        }
        self.update_model_snapshot();
    }

    fn update_model_snapshot(&self) {
        let all = self.models.get_models(None);
        let mut snapshot = write(&self.snapshot);
        let configured = std::mem::take(&mut snapshot.configured_providers);
        snapshot.available = all
            .iter()
            .filter(|model| configured.contains(&model.provider))
            .cloned()
            .collect();
        snapshot.configured_providers = configured;
        snapshot.all = all;
    }

    /// `checkProviderAuth` (models.ts:365-386).
    async fn check_provider_auth(
        &self,
        provider: &Arc<dyn Provider>,
    ) -> Result<Option<AuthCheck>, ModelsError> {
        let credential = self
            .credentials
            .read(provider.id())
            .await
            .map_err(|error| {
                ModelsError::with_cause(
                    ModelsErrorCode::Auth,
                    format!("Credential store read failed for {}", provider.id()),
                    &error.message,
                )
            })?;

        if let Some(Credential::OAuth(_)) = &credential {
            return Ok(provider.auth().oauth.as_ref().map(|_| AuthCheck {
                source: Some("OAuth".to_owned()),
                kind: AuthType::Oauth,
            }));
        }

        let Some(api_key) = provider.auth().api_key.clone() else {
            return Ok(None);
        };
        let auth_context: Arc<dyn AuthContext> = Arc::new(DefaultAuthContext);
        let api_key_credential = match &credential {
            Some(Credential::ApiKey(api_key_credential)) => Some(api_key_credential.clone()),
            _ => None,
        };
        // `apiKey.check` — the pir-ai trait cannot express "method absent"
        // (the default returns None), so a `None` result falls through to the
        // resolve chain. For env-var providers the two are equivalent.
        if let Some(check) = api_key
            .check(auth_context.as_ref(), api_key_credential.as_ref())
            .await
            .map_err(|error| {
                ModelsError::with_cause(
                    ModelsErrorCode::Auth,
                    format!("API key auth check failed for provider {}", provider.id()),
                    &error.message,
                )
            })?
        {
            return Ok(Some(check));
        }

        let credentials: Arc<dyn CredentialStore> = self.credentials.clone();
        let resolution = resolve_provider_auth(
            provider.id(),
            provider.auth(),
            &credentials,
            &auth_context,
            None,
        )
        .await?;
        Ok(resolution.map(|result| AuthCheck {
            source: result.source,
            kind: AuthType::ApiKey,
        }))
    }

    /// `runAvailabilityRefresh` (model-runtime.ts:240-268). Records the last
    /// failure for `get_error` (upstream `availabilityError`).
    async fn refresh_availability(&self) -> Result<(), ModelsError> {
        let result = self.refresh_availability_inner().await;
        *lock(&self.availability_error) = result.as_ref().err().map(|error| error.message.clone());
        result
    }

    async fn refresh_availability_inner(&self) -> Result<(), ModelsError> {
        let providers = self.models.get_providers();
        let mut auth = HashMap::new();
        for provider in &providers {
            if let Some(check) = self.check_provider_auth(provider).await? {
                auth.insert(provider.id().to_owned(), check);
            }
        }
        let stored: HashSet<String> = self
            .credentials
            .list()
            .await?
            .into_iter()
            .map(|entry| entry.provider_id)
            .collect();
        let configured: HashSet<String> = auth.keys().cloned().collect();
        let all = self.models.get_models(None);
        // `Models.getAvailable` (models.ts:394-408): configured providers
        // contribute their catalog after the credential-aware
        // `filterModels` policy (e.g. github-copilot subscription
        // narrowing).
        let mut available = Vec::new();
        for provider in &providers {
            if !configured.contains(provider.id()) {
                continue;
            }
            let credential = self
                .credentials
                .read(provider.id())
                .await
                .map_err(|error| {
                    ModelsError::with_cause(
                        ModelsErrorCode::Auth,
                        format!("Credential store read failed for {}", provider.id()),
                        &error.message,
                    )
                })?;
            available.extend(provider.filter_models(provider.get_models(), credential.as_ref()));
        }

        let mut snapshot = write(&self.snapshot);
        snapshot.all = all;
        snapshot.available = available;
        snapshot.configured_providers = configured;
        snapshot.stored_providers = stored;
        snapshot.auth = auth;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Catalog access
    // ------------------------------------------------------------------

    pub fn get_providers(&self) -> Vec<Arc<dyn Provider>> {
        self.models.get_providers()
    }

    pub fn get_provider(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        self.models.get_provider(provider_id)
    }

    pub fn get_models(&self, provider_id: Option<&str>) -> Vec<Model> {
        self.models.get_models(provider_id)
    }

    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<Model> {
        self.models.get_model(provider_id, model_id)
    }

    /// `checkAuth` (model-runtime.ts:309-311).
    pub async fn check_auth(&self, provider_id: &str) -> Result<Option<AuthCheck>, ModelsError> {
        let Some(provider) = self.models.get_provider(provider_id) else {
            return Ok(None);
        };
        self.check_provider_auth(&provider).await
    }

    /// `getAvailable` (model-runtime.ts:313-328): every call recomputes
    /// availability (upstream queues a refresh when none is in flight).
    pub async fn get_available(
        &self,
        provider_id: Option<&str>,
    ) -> Result<Vec<Model>, ModelsError> {
        self.refresh_availability().await?;
        let snapshot = read(&self.snapshot);
        Ok(match provider_id {
            Some(provider_id) => snapshot
                .available
                .iter()
                .filter(|model| model.provider == provider_id)
                .cloned()
                .collect(),
            None => snapshot.available.clone(),
        })
    }

    /// `getAvailableSnapshot` (model-runtime.ts:330-332).
    pub fn get_available_snapshot(&self) -> Vec<Model> {
        read(&self.snapshot).available.clone()
    }

    /// `getError` (model-runtime.ts:334-343).
    pub fn get_error(&self) -> Option<String> {
        let mut errors: Vec<String> = Vec::new();
        if let Some(config_error) = lock(&self.config).error() {
            errors.push(config_error.to_owned());
        }
        for (provider_id, error) in lock(&self.composition_errors).iter() {
            errors.push(format!("Provider \"{provider_id}\": {error}"));
        }
        if let Some(availability_error) = lock(&self.availability_error).as_ref() {
            errors.push(format!("Availability refresh: {availability_error}"));
        }
        if errors.is_empty() {
            None
        } else {
            Some(errors.join("\n\n"))
        }
    }

    pub fn get_registered_provider_config(&self, provider_id: &str) -> Option<ProviderConfigInput> {
        lock(&self.extension_providers).get(provider_id).cloned()
    }

    pub fn get_registered_provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for id in lock(&self.extension_providers).keys() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        for id in lock(&self.native_providers).keys() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }

    pub fn get_registered_native_provider(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        lock(&self.native_providers).get(provider_id).cloned()
    }

    /// `isUsingOAuth` (model-runtime.ts:366-368).
    pub fn is_using_oauth(&self, provider_id: &str) -> bool {
        read(&self.snapshot)
            .auth
            .get(provider_id)
            .map(|check| check.kind == AuthType::Oauth)
            .unwrap_or(false)
    }

    /// `hasConfiguredAuth` (model-runtime.ts:370-372).
    pub fn has_configured_auth(&self, provider_id: &str) -> bool {
        read(&self.snapshot)
            .configured_providers
            .contains(provider_id)
    }

    /// `getAuth(model, overrides)` (model-runtime.ts:374-396). Provider-level
    /// configured headers/`authHeader` are baked into the composed provider's
    /// auth resolution ([`ConfiguredApiKeyAuth`]); model-level headers merge
    /// in `Models::get_auth`.
    pub async fn get_auth(
        &self,
        model: &Model,
        overrides: Option<&ModelRuntimeAuthOverrides>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        let overrides = overrides.cloned().unwrap_or_default();
        self.models
            .get_auth(
                model,
                Some(&AuthResolutionOverrides {
                    api_key: overrides.api_key.clone(),
                    env: overrides.env.clone(),
                }),
            )
            .await
    }

    /// `getAuth(providerId)` (model-runtime.ts:380 string arm) — provider-level
    /// auth resolution without a model (the llama.cpp extension's
    /// `modelRegistry.getProviderAuth`, T14 W6b).
    pub async fn get_provider_auth(
        &self,
        provider_id: &str,
    ) -> Result<Option<AuthResult>, ModelsError> {
        self.models.get_provider_auth(provider_id, None).await
    }

    /// `setRuntimeApiKey` (model-runtime.ts:398-415) — non-persistent
    /// runtime override (the `--api-key` CLI path). Like upstream, ends in
    /// `refresh`: models.json is reloaded and providers rebuilt.
    pub async fn set_runtime_api_key(&self, provider_id: &str, api_key: &str) {
        self.credentials.set_runtime_api_key(provider_id, api_key);
        self.refresh(None).await;
    }

    /// `removeRuntimeApiKey` (model-runtime.ts:417-420).
    pub async fn remove_runtime_api_key(&self, provider_id: &str) {
        self.credentials.remove_runtime_api_key(provider_id);
        self.refresh(Some(ModelsRefreshOptions {
            allow_network: Some(self.model_network_enabled),
            force: None,
            signal: None,
        }))
        .await;
    }

    /// `listCredentials` (model-runtime.ts:422-424).
    pub async fn list_credentials(&self) -> Result<Vec<CredentialInfo>, ModelsError> {
        self.credentials.list().await
    }

    /// `login` (model-runtime.ts:503-507): run the provider's login through
    /// `Models`, then refresh so the new credential takes effect in model
    /// availability and provider composition.
    pub async fn login(
        &self,
        provider_id: &str,
        auth_type: AuthType,
        interaction: &dyn AuthInteraction,
    ) -> Result<Credential, ModelsError> {
        let credential = self
            .models
            .login(provider_id, auth_type, interaction)
            .await?;
        self.refresh(Some(ModelsRefreshOptions {
            allow_network: Some(self.model_network_enabled),
            force: None,
            signal: None,
        }))
        .await;
        Ok(credential)
    }

    /// `logout` (model-runtime.ts:509-514): remove the stored credential,
    /// reset credential-dependent compatibility projections, then refresh so
    /// the unconfigured provider is skipped by availability.
    pub async fn logout(&self, provider_id: &str) -> Result<(), ModelsError> {
        self.models.logout(provider_id).await?;
        self.recompose_provider(provider_id);
        self.refresh(Some(ModelsRefreshOptions {
            allow_network: Some(self.model_network_enabled),
            force: None,
            signal: None,
        }))
        .await;
        Ok(())
    }

    pub fn has_runtime_api_key(&self, provider_id: &str) -> bool {
        self.credentials.has_runtime_api_key(provider_id)
    }

    // ------------------------------------------------------------------
    // Streaming (prepareRequest + lazyStream, model-runtime.ts:438-501)
    // ------------------------------------------------------------------

    /// `stream` — delegates to [`Models::stream`], which performs the same
    /// lazy auth resolution + header merge as upstream `prepareRequest`.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.models.stream(model, context, options)
    }

    pub async fn complete(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> Option<AssistantMessage> {
        self.models.complete(model, context, options).await
    }

    /// `streamSimple` (model-runtime.ts:492-497).
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsSimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.models.stream_simple(model, context, options)
    }

    pub async fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsSimpleStreamOptions>,
    ) -> Option<AssistantMessage> {
        self.models.complete_simple(model, context, options).await
    }

    /// `refresh(options)` (model-runtime.ts:516-537): reload models.json,
    /// rebuild providers, refresh dynamic model catalogs, recompute
    /// availability. `allowNetwork` defaults to `model_network_enabled`
    /// (upstream `options.allowNetwork ?? this.modelNetworkEnabled`).
    pub async fn refresh(&self, options: Option<ModelsRefreshOptions>) -> ModelsRefreshResult {
        let config = ModelConfig::load(self.models_path.as_deref()).await;
        *lock(&self.config) = config;
        self.rebuild_providers();
        let refresh_options = ModelsRefreshOptions {
            allow_network: Some(
                options
                    .as_ref()
                    .and_then(|o| o.allow_network)
                    .unwrap_or(self.model_network_enabled),
            ),
            force: options.as_ref().and_then(|o| o.force),
            signal: options.as_ref().and_then(|o| o.signal.clone()),
        };
        let result = self.models.refresh(Some(refresh_options)).await;
        self.update_model_snapshot();
        if let Err(error) = self.refresh_availability().await {
            // Availability errors are recorded in `get_error`; refreshed
            // models remain usable (model-runtime.ts:531-535).
            tracing::warn!("availability refresh failed: {}", error.message);
        }
        result
    }

    /// `registerNativeProvider` (model-runtime.ts:539-546). Upstream kicks a
    /// fire-and-forget refresh; pir awaits it so `has_configured_auth` is
    /// settled when the call returns (headless flows are single-task).
    pub async fn register_native_provider(
        &self,
        provider: Arc<dyn Provider>,
    ) -> Result<(), String> {
        if provider.id().trim().is_empty() {
            return Err("Provider id must not be empty.".to_owned());
        }
        let id = provider.id().to_owned();
        lock(&self.extension_providers).remove(&id);
        lock(&self.native_providers).insert(id.clone(), provider);
        self.recompose_provider(&id);
        self.update_model_snapshot();
        self.refresh(Some(ModelsRefreshOptions {
            allow_network: Some(false),
            force: None,
            signal: None,
        }))
        .await;
        Ok(())
    }

    /// `registerProvider` (model-runtime.ts:548-584): re-registration merges
    /// defined values over the previous registration.
    pub async fn register_provider(
        &self,
        provider_id: &str,
        config: ProviderConfigInput,
    ) -> Result<(), String> {
        // Validate the incoming registration on its own, like upstream: a
        // broken re-registration must fail without touching the stored config.
        Self::compose_provider(
            provider_id,
            lock(&self.native_providers).get(provider_id),
            lock(&self.config).get_provider(provider_id),
            Some(&config),
        )?;

        lock(&self.native_providers).remove(provider_id);
        let previous = lock(&self.extension_providers).remove(provider_id);
        let effective = merge_provider_config(previous, config);
        lock(&self.extension_providers).insert(provider_id.to_owned(), effective);
        self.recompose_provider(provider_id);
        self.update_model_snapshot();
        self.refresh(Some(ModelsRefreshOptions {
            allow_network: Some(false),
            force: None,
            signal: None,
        }))
        .await;
        Ok(())
    }

    /// `unregisterProvider` (model-runtime.ts:586-592).
    pub async fn unregister_provider(&self, provider_id: &str) {
        lock(&self.extension_providers).remove(provider_id);
        lock(&self.native_providers).remove(provider_id);
        self.recompose_provider(provider_id);
        self.update_model_snapshot();
        self.refresh(Some(ModelsRefreshOptions {
            allow_network: Some(false),
            force: None,
            signal: None,
        }))
        .await;
    }
}

/// Re-registration merge (model-runtime.ts:555-560): defined values of the
/// new config win; undefined ones inherit the previous registration.
fn merge_provider_config(
    previous: Option<ProviderConfigInput>,
    config: ProviderConfigInput,
) -> ProviderConfigInput {
    let previous = previous.unwrap_or_default();
    ProviderConfigInput {
        name: config.name.or(previous.name),
        base_url: config.base_url.or(previous.base_url),
        api_key: config.api_key.or(previous.api_key),
        api: config.api.or(previous.api),
        headers: config.headers.or(previous.headers),
        auth_header: config.auth_header.or(previous.auth_header),
        models: config.models.or(previous.models),
    }
}

fn default_input() -> Vec<pir_ai::types::InputModality> {
    vec![pir_ai::types::InputModality::Text]
}

/// models.json model definition → runtime `Model` (`modelFromJson`,
/// provider-composer.ts:124-159). Structural errors are composition errors,
/// like upstream throwing out of `getModels`.
fn json_model_to_model(
    provider_id: &str,
    provider_api: &Option<String>,
    provider_base_url: Option<&str>,
    provider_compat: Option<&ModelCompat>,
    model: ModelsJsonModel,
) -> Result<Model, String> {
    // JS `if (!api)` — empty strings are falsy too.
    let api = model
        .api
        .clone()
        .or_else(|| provider_api.clone())
        .filter(|api| !api.is_empty());
    let Some(api) = api else {
        return Err(format!(
            "Provider {provider_id}, model {}: no \"api\" specified. Set at provider or model level.",
            model.id
        ));
    };
    let base_url = model
        .base_url
        .clone()
        .or_else(|| provider_base_url.map(str::to_owned))
        .filter(|url| !url.is_empty());
    let Some(base_url) = base_url else {
        return Err(format!(
            "Provider {provider_id}: \"baseUrl\" is required when defining custom models."
        ));
    };
    if let Some(context_window) = model.context_window {
        if context_window <= 0.0 {
            return Err(format!(
                "Provider {provider_id}, model {}: invalid contextWindow",
                model.id
            ));
        }
    }
    if let Some(max_tokens) = model.max_tokens {
        if max_tokens <= 0.0 {
            return Err(format!(
                "Provider {provider_id}, model {}: invalid maxTokens",
                model.id
            ));
        }
    }
    Ok(Model {
        name: model.name.clone().unwrap_or_else(|| model.id.clone()),
        api: ApiKind::from(api),
        provider: provider_id.to_owned(),
        base_url,
        reasoning: model.reasoning.unwrap_or(false),
        thinking_level_map: model.thinking_level_map.clone(),
        input: model.input.clone().unwrap_or_else(default_input),
        cost: model.cost.clone().unwrap_or_default(),
        context_window: model.context_window.unwrap_or(128000.0) as u32,
        max_tokens: model.max_tokens.unwrap_or(16384.0) as u32,
        headers: model
            .headers
            .as_ref()
            .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        compat: match model.compat.clone() {
            Some(compat) => Some(merge_compat(provider_compat, &compat)),
            None => provider_compat.cloned(),
        },
        id: model.id,
    })
}

/// Extension `ProviderConfigModel` → runtime `Model` (`applyExtension`,
/// provider-composer.ts:201-228).
fn config_model_to_model(
    provider_id: &str,
    provider_api: &Option<String>,
    provider_base_url: Option<&str>,
    model: ProviderConfigModel,
) -> Result<Model, String> {
    let api = model
        .api
        .clone()
        .or_else(|| provider_api.clone())
        .filter(|api| !api.is_empty());
    let Some(api) = api else {
        return Err(format!(
            "Provider {provider_id}, model {}: no \"api\" specified. Set at provider or model level.",
            model.id
        ));
    };
    let base_url = model
        .base_url
        .clone()
        .or_else(|| provider_base_url.map(str::to_owned))
        .filter(|url| !url.is_empty());
    let Some(base_url) = base_url else {
        return Err(format!(
            "Provider {provider_id}: \"baseUrl\" is required when defining custom models."
        ));
    };
    Ok(Model {
        name: model.name.clone().unwrap_or_else(|| model.id.clone()),
        api: ApiKind::from(api),
        provider: provider_id.to_owned(),
        base_url,
        reasoning: model.reasoning,
        thinking_level_map: model.thinking_level_map.clone(),
        input: if model.input.is_empty() {
            default_input()
        } else {
            model.input.clone()
        },
        cost: model.cost.clone().unwrap_or_default(),
        context_window: model.context_window,
        max_tokens: model.max_tokens,
        headers: model.headers.clone(),
        id: model.id,
        compat: model.compat.clone(),
    })
}

// ============================================================================
// Tests: models.json composition layer (provider-composer.ts parity anchors)
// ============================================================================

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::io::AsyncReadExt;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "pir-model-runtime-test-{}-{nanos}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn runtime_with_models_json(models_json: &str) -> (TempDir, Arc<ModelRuntime>) {
        let tmp = TempDir::new();
        let agent_dir = tmp.0.join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(agent_dir.join("models.json"), models_json).expect("write models.json");
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
            ..Default::default()
        })
        .await;
        (tmp, runtime)
    }

    /// Upstream defaults (`modelFromJson`, provider-composer.ts:154-155):
    /// 128000 / 16384 — never 0 (a 0 window would clamp requests to
    /// `max_tokens: 1`).
    #[tokio::test]
    async fn model_defaults_context_window_and_max_tokens() {
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "PIR_TEST_COMPOSE_DEFAULTS_KEY",
                "models": [{"id": "m1"}]
            }}}"#,
        )
        .await;
        let model = runtime.get_model("custom", "m1").expect("model m1");
        assert_eq!(model.context_window, 128000);
        assert_eq!(model.max_tokens, 16384);
    }

    /// Missing api/baseUrl are composition errors (provider-composer.ts:131-137),
    /// not silently broken models.
    #[tokio::test]
    async fn missing_api_or_base_url_is_a_composition_error() {
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {
                "no-api": {
                    "baseUrl": "https://api.example.com/v1",
                    "apiKey": "PIR_TEST_COMPOSE_NOAPI_KEY",
                    "models": [{"id": "m1"}]
                },
                "no-base-url": {
                    "api": "openai-completions",
                    "apiKey": "PIR_TEST_COMPOSE_NOURL_KEY",
                    "models": [{"id": "m1"}]
                }
            }}"#,
        )
        .await;
        assert!(runtime.get_model("no-api", "m1").is_none());
        assert!(runtime.get_model("no-base-url", "m1").is_none());
        let error = runtime.get_error().expect("composition errors");
        assert!(
            error.contains("no \"api\" specified"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("\"baseUrl\" is required"),
            "unexpected error: {error}"
        );
    }

    /// Explicit non-positive window/budgets are rejected
    /// (provider-composer.ts:138-143).
    #[tokio::test]
    async fn non_positive_window_or_budget_is_a_composition_error() {
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "PIR_TEST_COMPOSE_INVALID_KEY",
                "models": [{"id": "m1", "contextWindow": 0}]
            }}}"#,
        )
        .await;
        let error = runtime.get_error().expect("composition error");
        assert!(
            error.contains("invalid contextWindow"),
            "unexpected error: {error}"
        );
    }

    /// `modelOverrides` apply over constructed models
    /// (provider-composer.ts:100-122, 434-437).
    #[tokio::test]
    async fn model_overrides_apply() {
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "PIR_TEST_COMPOSE_OVERRIDE_KEY",
                "models": [{"id": "m1", "contextWindow": 50000}],
                "modelOverrides": {
                    "m1": {
                        "name": "Overridden",
                        "reasoning": true,
                        "contextWindow": 64000,
                        "maxTokens": 4096
                    }
                }
            }}}"#,
        )
        .await;
        let model = runtime.get_model("custom", "m1").expect("model m1");
        assert_eq!(model.name, "Overridden");
        assert!(model.reasoning);
        assert_eq!(model.context_window, 64000);
        assert_eq!(model.max_tokens, 4096);
    }

    /// Provider-level `headers` and `authHeader` wrap auth resolution, so the
    /// stream path (which resolves through the composed provider auth) sends
    /// them too (`withConfiguredAuth`, provider-composer.ts:250-262).
    #[tokio::test]
    async fn provider_headers_and_auth_header_reach_resolved_auth() {
        const ENV_KEY: &str = "PIR_TEST_COMPOSE_HEADERS_KEY";
        std::env::set_var(ENV_KEY, "test-api-key");
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "PIR_TEST_COMPOSE_HEADERS_KEY",
                "headers": {"X-Custom": "yes"},
                "authHeader": true,
                "models": [{"id": "m1"}]
            }}}"#,
        )
        .await;
        let model = runtime.get_model("custom", "m1").expect("model m1");
        let auth = runtime
            .get_auth(&model, None)
            .await
            .expect("get_auth")
            .expect("auth resolved");
        let headers = auth.auth.headers.expect("headers");
        assert_eq!(
            headers.get("X-Custom").and_then(|v| v.as_deref()),
            Some("yes")
        );
        assert_eq!(
            headers.get("Authorization").and_then(|v| v.as_deref()),
            Some("Bearer test-api-key")
        );
        std::env::remove_var(ENV_KEY);
    }

    /// Provider enumeration order is the models.json insertion order
    /// (upstream JS `Map` semantics), not hash order.
    #[tokio::test]
    async fn provider_order_is_insertion_order() {
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {
                "zeta": {
                    "baseUrl": "https://z.example.com",
                    "api": "openai-completions",
                    "apiKey": "PIR_TEST_COMPOSE_ORDER_KEY",
                    "models": [{"id": "m1"}]
                },
                "alpha": {
                    "baseUrl": "https://a.example.com",
                    "api": "openai-completions",
                    "apiKey": "PIR_TEST_COMPOSE_ORDER_KEY",
                    "models": [{"id": "m1"}]
                }
            }}"#,
        )
        .await;
        let order: Vec<String> = runtime
            .get_models(None)
            .into_iter()
            .map(|model| model.provider)
            .collect();
        assert_eq!(order, vec!["zeta".to_owned(), "alpha".to_owned()]);
    }

    // ------------------------------------------------------------------
    // W6-C: dynamic catalog refresh plumbing (model-runtime.ts:161-170,
    // 516-537)
    // ------------------------------------------------------------------

    #[test]
    fn default_model_refresh_timeout_is_15_seconds() {
        // The create-time / `update --models` timeout default
        // (model-runtime.ts:164, package-manager-cli.ts:404).
        assert_eq!(DEFAULT_MODEL_REFRESH_TIMEOUT_MS, 15_000);
    }

    /// A hung loopback catalog endpoint: accepts the request, reads the
    /// head, then never responds — the refresh must abort via the signal.
    async fn hung_catalog_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                // Never respond; keep the connection open.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
        url
    }

    /// The shared refresh signal aborts a hanging catalog fetch
    /// (`update --models` / create-time timeout mechanism): the result
    /// reports `aborted` and no error is recorded.
    #[tokio::test]
    async fn refresh_aborts_hanging_catalog_fetch_via_signal() {
        const ENV_KEY: &str = "PIR_TEST_MODEL_RUNTIME_REFRESH_KEY";
        std::env::set_var(ENV_KEY, "test-key");
        let url = hung_catalog_server().await;
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: None,
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        let inner = create_provider(CreateProviderOptions {
            id: "remote-catalog-test".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(env_api_key_auth("Test API key", &[ENV_KEY]))),
                oauth: None,
            },
            models: vec![],
            api: ProviderApi::Single(Arc::new(pir_ai::api::openai_completions::OpenAiCompletions)),
        });
        runtime
            .register_native_provider(crate::core::remote_catalog_provider::with_remote_catalog(
                inner,
                Some(url),
                None,
            ))
            .await
            .expect("register");

        let token = CancellationToken::new();
        let abort = token.clone();
        let timeout = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            abort.cancel();
        });
        let result = runtime
            .refresh(Some(ModelsRefreshOptions {
                allow_network: Some(true),
                force: Some(true),
                signal: Some(token),
            }))
            .await;
        timeout.await.expect("timeout task");
        assert!(result.aborted);
        assert!(result.errors.is_empty());
        std::env::remove_var(ENV_KEY);
    }

    /// Runtime-path coverage for `filterModels` (models.ts:394-408 via
    /// model-runtime.ts:240-268): `get_available` narrows the github-copilot
    /// catalog through the credential's `availableModelIds` — not just at
    /// the provider unit level (`oauth_copilot_radius.rs`).
    #[tokio::test]
    async fn get_available_applies_copilot_filter_models() {
        let store = Arc::new(pir_ai::auth::credential_store::InMemoryCredentialStore::new());
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(store.clone()),
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        let provider = pir_ai::providers::github_copilot::github_copilot_provider();
        let full_catalog = provider.get_models();
        assert!(full_catalog.len() > 1);
        runtime
            .register_native_provider(provider)
            .await
            .expect("register");

        // No credential → provider not configured → nothing available.
        assert!(runtime
            .get_available(None)
            .await
            .expect("get_available")
            .is_empty());

        // A credential exactly as `GitHubCopilotOAuth::login`/`refresh`
        // produces it (extras: `enterpriseUrl`, `availableModelIds`).
        let kept = full_catalog[0].id.clone();
        let mut extra = serde_json::Map::new();
        extra.insert(
            "enterpriseUrl".to_owned(),
            serde_json::json!("company.ghe.com"),
        );
        extra.insert(
            "availableModelIds".to_owned(),
            serde_json::json!([kept, "ghost-model"]),
        );
        let credential = Credential::OAuth(pir_ai::auth::types::OAuthCredential {
            refresh: "ghu_refresh_token".to_owned(),
            access: "tid=test;exp=9999999999;".to_owned(),
            expires: i64::MAX,
            extra,
        });
        store
            .modify(
                "github-copilot",
                Arc::new(move |_| {
                    let credential = credential.clone();
                    Box::pin(async move { Ok(Some(credential)) })
                }),
            )
            .await
            .expect("modify");

        let available = runtime.get_available(None).await.expect("get_available");
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, full_catalog[0].id);
        // The complete synchronous catalog remains intact.
        assert_eq!(runtime.get_models(None).len(), full_catalog.len());
    }
}
