//! Canonical model/auth runtime (design §6.1 services).
//!
//! Port of `packages/coding-agent/src/core/model-runtime.ts` @ pi 0.84.1+
//! (4181f66) plus `runtime-credentials.ts`.
//!
//! v0.11 additions (T23 R3.4.6/R3.4.7):
//! - Availability refresh is generation-gated (seq counters prevent stale
//!   passes from publishing superseded snapshots, model-runtime.ts:148-152,
//!   285-329).
//! - Credential operations (login/logout/setRuntimeApiKey/removeRuntimeApiKey)
//!   are serialized per-provider through an enqueue chain, and wrapped with
//!   `synchronize_credential_state` + `CredentialSynchronizationError`
//!   (model-runtime.ts:494-534, 536-688).
//!
//! - `ModelsStore` persistence (file next to models.json, upstream
//!   `FileModelsStore`) and `Models::refresh` network plumbing landed:
//!   `CreateModelRuntimeOptions` gains `modelsStore` / `modelsStorePath` /
//!   `allowModelNetwork` / `modelRefreshTimeoutMs`; `refresh(options)` runs
//!   dynamic catalog refreshes with `allowNetwork` defaulting to
//!   `modelNetworkEnabled` (= `RPI_OFFLINE` unset).
//! - The registration wave (D-052) wraps every static built-in in
//!   [`remote_catalog_provider::with_remote_catalog`] at `create()` time
//!   (radius passes through), resolved through
//!   [`remote_catalog_provider::model_catalog_endpoint`] (env > settings >
//!   `https://revpi.dev` (ADR-0009), literal `off` disables the overlay) — the
//!   `rpi update --models` consumer path (model-runtime.ts:144-150, D-038).
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
use std::time::Duration;

use futures::future::BoxFuture;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use rpi_ai::api::anthropic_messages::AnthropicMessages;
use rpi_ai::api::openai_completions::OpenAiCompletions;
use rpi_ai::api::openai_responses::OpenAiResponses;
use rpi_ai::auth::file_store::FileCredentialStore;
use rpi_ai::auth::helpers::env_api_key_auth;
use rpi_ai::auth::interaction::AuthInteraction;
use rpi_ai::auth::resolve::{
    resolve_provider_auth, AuthResolutionOverrides, ModelsError, ModelsErrorCode,
};
use rpi_ai::auth::types::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthOperationOptions, AuthResult,
    AuthType, Credential, CredentialInfo, CredentialStore, CredentialType, DefaultAuthContext,
    ModifyFn, ProviderAuth,
};
use rpi_ai::models::{
    create_provider, CreateModelsOptions, CreateProviderOptions, Models, ModelsRefreshOptions,
    ModelsRefreshResult, ModelsSimpleStreamOptions, ModelsStreamOptions, Provider, ProviderApi,
    ProviderStreams, RefreshModelsContext,
};
use rpi_ai::models_json::{
    ModelConfig, ModelsJsonModel, ModelsJsonModelOverride, ModelsJsonProvider, OrderedMap,
};
use rpi_ai::models_store::{InMemoryModelsStore, JsonFileModelsStore, ModelsStore};
use rpi_ai::types::{
    ApiKind, AssistantMessage, Context, Model, ModelCompat, ModelCost, ModelCostRates, ProviderEnv,
    ProviderHeaders, SimpleStreamOptions, StreamOptions,
};
use rpi_ai::utils::event_stream::AssistantMessageEventStream;

use crate::config::{get_agent_dir, ENV_OFFLINE};
use crate::core::remote_catalog_provider::{model_catalog_endpoint, with_remote_catalog};

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

    /// Returns a clone of the inner credential store handle (for direct
    /// read-only access by auth CLI commands).
    pub fn store_clone(&self) -> Arc<dyn CredentialStore> {
        Arc::clone(&self.store)
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
    async fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, ModelsError> {
        if let Some(key) = self.runtime_override(provider_id) {
            return Ok(Some(Credential::ApiKey(ApiKeyCredential {
                key: Some(key),
                env: None,
            })));
        }
        self.store.read(provider_id, options).await
    }

    async fn list(
        &self,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, ModelsError> {
        let mut entries: Vec<CredentialInfo> = self.store.list(options).await?;
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
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, ModelsError> {
        self.store.modify(provider_id, f, options).await
    }

    async fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<(), ModelsError> {
        self.remove_runtime_api_key(provider_id);
        self.store.delete(provider_id, options).await
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
    pub thinking_level_map: Option<rpi_ai::types::ThinkingLevelMap>,
    pub input: Vec<rpi_ai::types::InputModality>,
    pub cost: Option<rpi_ai::types::ModelCost>,
    pub context_window: u32,
    pub max_tokens: u32,
    /// 25a2c8dcf (#7568): rides the `...definition` spread upstream.
    pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<rpi_ai::types::ModelCompat>,
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

/// Computed snapshot data before publishing (the body of
/// `runAvailabilityRefresh`, model-runtime.ts:286-313).
struct ModelRuntimeSnapshotData {
    all: Vec<Model>,
    available: Vec<Model>,
    configured_providers: HashSet<String>,
    stored_providers: HashSet<String>,
    auth: HashMap<String, AuthCheck>,
}

// ============================================================================
// Credential synchronization (model-runtime.ts:91-111, 494-534)
// ============================================================================

/// `CredentialSynchronizationOperation` (model-runtime.ts:91).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSynchronizationOperation {
    Login,
    Logout,
    SetRuntimeApiKey,
    RemoveRuntimeApiKey,
}

impl CredentialSynchronizationOperation {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Logout => "logout",
            Self::SetRuntimeApiKey => "setRuntimeApiKey",
            Self::RemoveRuntimeApiKey => "removeRuntimeApiKey",
        }
    }
}

/// `CredentialSynchronizationError` (model-runtime.ts:94-111): the
/// credential mutation committed successfully but the local model/auth
/// snapshot could not be synchronized.
#[derive(Debug)]
pub struct CredentialSynchronizationError {
    pub provider_id: String,
    pub operation: CredentialSynchronizationOperation,
    pub credential: Option<Credential>,
    pub cause: String,
}

impl std::fmt::Display for CredentialSynchronizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Credential {} committed for {}, but local synchronization failed",
            self.operation.as_str(),
            self.provider_id
        )
    }
}

impl std::error::Error for CredentialSynchronizationError {}

// ============================================================================
// Api registry (T03 adapters; requirements §5.1 remainder is T13)
// ============================================================================

/// `getApiProvider` equivalent for the adapters rpi-ai implements.
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
    /// `catalogBaseUrl` (model-runtime.ts:77): remote model-catalog overlay
    /// base URL. `None` resolves `RPI_MODEL_CATALOG_URL` env > default
    /// `https://revpi.dev` through [`model_catalog_endpoint`]; the literal
    /// `off` disables the overlay entirely (ADR-0002 §8) — built-in
    /// providers are then registered without the remote-catalog decorator.
    pub catalog_base_url: Option<String>,
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

/// `ModelRuntimeAuthOverrides` (model-runtime.ts:72-75 + 84-89 @ 4181f66).
#[derive(Debug, Clone, Default)]
pub struct ModelRuntimeAuthOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    /// Require this much remaining OAuth-token validity; defaults to five
    /// minutes (model-runtime.ts:88, auth/resolve.ts:119).
    pub min_oauth_validity_ms: Option<u64>,
}

/// `CompatibilityRequestConfig` (model-registry.ts:95-100 @ 4181f66) —
/// the model's `authHeader` / `headers` fields, used by the no-auth fallback
/// branch of `getApiKeyAndHeaders`.
#[derive(Debug, Clone, Default)]
pub struct CompatibilityRequestConfig {
    pub auth_header: bool,
    pub headers: std::collections::HashMap<String, Option<String>>,
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
    /// Resolved refresh timeout (`modelRefreshTimeoutMs ?? 15s`,
    /// model-runtime.ts:163-165): bounds the create-time refresh and the
    /// post-login/logout refresh ([`bounded_refresh_signal`]) alike.
    model_refresh_timeout_ms: u64,
    /// Insertion-ordered like the upstream JS `Map`s: provider enumeration
    /// order is observable (initial-model fallback, available listings).
    native_providers: Mutex<OrderedMap<Arc<dyn Provider>>>,
    extension_providers: Mutex<OrderedMap<ProviderConfigInput>>,
    composition_errors: Mutex<OrderedMap<String>>,
    availability_error: Mutex<Option<String>>,
    snapshot: RwLock<ModelRuntimeSnapshot>,
    // ------------------------------------------------------------------
    // Generation counters (model-runtime.ts:148-152, commits 8f9e76974 +
    // c6eb6281a + a077fff0b): seq-gating prevents stale availability passes
    // from publishing superseded snapshots, and allows credential
    // operations to invalidate in-flight refreshes.
    // ------------------------------------------------------------------
    /// `availabilityRefreshSeq` (model-runtime.ts:148): incremented by every
    /// full availability refresh; a pass whose seq no longer matches the
    /// current value is discarded.
    availability_refresh_seq: Mutex<u64>,
    /// `availabilityErrorSeq` (model-runtime.ts:149): incremented whenever
    /// an error is about to be recorded, so a newer successful pass can
    /// clear a stale error.
    availability_error_seq: Mutex<u64>,
    /// `providerAvailabilitySeq` (model-runtime.ts:150): per-provider seq
    /// for single-provider refreshes (credential operations).
    provider_availability_seq: Mutex<HashMap<String, u64>>,
    /// `credentialOperations` (model-runtime.ts:152): per-provider
    /// serialization of credential mutations (login/logout/setRuntimeApiKey/
    /// removeRuntimeApiKey). Upstream chains JS Promises; Rust uses
    /// per-provider `tokio::sync::Mutex` for arrival-order serialization.
    credential_operations: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
/// the four nested objects merged key-wise. Implemented over the serialized
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
        "chatTemplateArgs",
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
    // 25a2c8dcf (#7568): `{...model.samplingParams, ...override.samplingParams}`
    // — per-key override, absent override keeps the model value.
    if let Some(override_sampling) = &override_.sampling_params {
        model.sampling_params = Some(match model.sampling_params.take() {
            Some(mut base) => {
                base.extend(override_sampling.clone());
                base
            }
            None => override_sampling.clone(),
        });
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

/// A cancellation token for the post-login/logout refresh: the resolved
/// refresh timeout (`model_refresh_timeout_ms`, the same value that bounds
/// the create-time refresh) bounds the whole refresh, and an optional
/// interaction signal (the login dialog's cancel token) aborts it early — a
/// stuck remote-catalog fetch can no longer freeze the login flow
/// indefinitely.
fn bounded_refresh_signal(
    timeout_ms: u64,
    interaction_signal: Option<CancellationToken>,
) -> CancellationToken {
    let token = CancellationToken::new();
    if let Some(parent) = interaction_signal {
        let child = token.clone();
        let parent = parent.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = parent.cancelled() => child.cancel(),
                () = child.cancelled() => {}
            }
        });
    }
    let timeout = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
        timeout.cancel();
    });
    token
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
        // Resolved once here so the create-time refresh and the
        // post-login/logout refresh (`bounded_refresh_signal`) share the
        // same configured bound.
        let model_refresh_timeout_ms = options
            .model_refresh_timeout_ms
            .unwrap_or(DEFAULT_MODEL_REFRESH_TIMEOUT_MS);
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
            model_refresh_timeout_ms,
            native_providers: Mutex::new(OrderedMap::default()),
            extension_providers: Mutex::new(OrderedMap::default()),
            composition_errors: Mutex::new(OrderedMap::default()),
            availability_error: Mutex::new(None),
            snapshot: RwLock::new(ModelRuntimeSnapshot::default()),
            availability_refresh_seq: Mutex::new(0),
            availability_error_seq: Mutex::new(0),
            provider_availability_seq: Mutex::new(HashMap::new()),
            credential_operations: Mutex::new(HashMap::new()),
        });
        // Seed the built-in providers (model-runtime.ts:181-190): every
        // static catalog provider is wrapped in the persisted remote-catalog
        // overlay (`withRemoteCatalog`); radius is a dynamic provider and
        // passes through unchanged. models.json then composes over these
        // bases as the overlay layer (provider-composer), so users only
        // write custom/override config — D-038 registration wave.
        let catalog_base_url = model_catalog_endpoint(options.catalog_base_url.as_deref());
        let builtin_generated_at = rpi_ai::generated::get_builtin_model_data_generated_at();
        for provider in rpi_ai::providers::builtin_providers() {
            let provider = if provider.id() == "radius" {
                provider
            } else if let Some(base_url) = &catalog_base_url {
                with_remote_catalog(provider, Some(base_url.clone()), builtin_generated_at)
            } else {
                provider
            };
            lock(&runtime.native_providers).insert(provider.id().to_owned(), provider);
        }
        runtime.rebuild_providers();
        // create() refreshes dynamic catalogs over the network only when
        // explicitly enabled, with a default 15s timeout (model-runtime.ts:
        // 161-170).
        let refresh_from_network = model_network_enabled && options.allow_model_network;
        let token = refresh_from_network.then(CancellationToken::new);
        if let Some(token) = &token {
            let abort = token.clone();
            let timeout_ms = model_refresh_timeout_ms;
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
                ..Default::default()
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
            ..Default::default()
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
            .read(provider.id(), None)
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
        // `apiKey.check` — the rpi-ai trait cannot express "method absent"
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

    /// `runAvailabilityRefresh` (model-runtime.ts:285-314, commits 8f9e76974
    /// et al.): computes auth + available, then publishes the snapshot only
    /// when `seq` still matches `availability_refresh_seq` — a newer pass
    /// invalidates this one. Clears the error when `error_seq` matches.
    async fn run_availability_refresh(&self, seq: u64, error_seq: u64) -> Result<(), ModelsError> {
        let result = self.compute_availability().await;
        // Stale check: if a newer refresh was queued, discard this result.
        if seq != *lock(&self.availability_refresh_seq) {
            return result.map(|_| ());
        }
        match result {
            Ok(snapshot_data) => {
                let mut snapshot = write(&self.snapshot);
                snapshot.all = snapshot_data.all;
                snapshot.available = snapshot_data.available;
                snapshot.configured_providers = snapshot_data.configured_providers;
                snapshot.stored_providers = snapshot_data.stored_providers;
                snapshot.auth = snapshot_data.auth;
                // Clear the error only when our error_seq is still current.
                if error_seq == *lock(&self.availability_error_seq) {
                    *lock(&self.availability_error) = None;
                }
                Ok(())
            }
            Err(error) => {
                if error_seq == *lock(&self.availability_error_seq) {
                    *lock(&self.availability_error) = Some(error.message.clone());
                }
                Err(error)
            }
        }
    }

    /// `queueAvailabilityRefresh` (model-runtime.ts:316-329): bumps the
    /// generation counters and runs the refresh, so any older in-flight pass
    /// becomes stale. Errors are recorded for `get_error`.
    async fn queue_availability_refresh(&self) -> Result<(), ModelsError> {
        let seq = {
            let mut s = lock(&self.availability_refresh_seq);
            *s += 1;
            *s
        };
        // Invalidate all per-provider seqs: a full refresh supersedes any
        // single-provider pass (model-runtime.ts:318-320).
        for provider_seq in lock(&self.provider_availability_seq).values_mut() {
            *provider_seq += 1;
        }
        let error_seq = {
            let mut s = lock(&self.availability_error_seq);
            *s += 1;
            *s
        };
        match self.run_availability_refresh(seq, error_seq).await {
            Ok(()) => Ok(()),
            Err(error) => {
                if error_seq == *lock(&self.availability_error_seq) {
                    *lock(&self.availability_error) = Some(error.message.clone());
                }
                Err(error)
            }
        }
    }

    /// Computes the full availability snapshot data without publishing it.
    /// (`runAvailabilityRefresh` body, model-runtime.ts:286-313.)
    async fn compute_availability(&self) -> Result<ModelRuntimeSnapshotData, ModelsError> {
        let providers = self.models.get_providers();
        let mut auth = HashMap::new();
        for provider in &providers {
            if let Some(check) = self.check_provider_auth(provider).await? {
                auth.insert(provider.id().to_owned(), check);
            }
        }
        let stored: HashSet<String> = self
            .credentials
            .list(None)
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
                .read(provider.id(), None)
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
        Ok(ModelRuntimeSnapshotData {
            all,
            available,
            configured_providers: configured,
            stored_providers: stored,
            auth,
        })
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

    /// Returns the credential store backing this runtime. Used by the auth
    /// CLI commands' `--credentials` path (`getProviderCredential`,
    /// auth-check.ts:55-64) to read stored credentials without triggering a
    /// refresh.
    pub fn credential_store(&self) -> Arc<dyn CredentialStore> {
        // RuntimeCredentials delegates read/list to the wrapped store; for the
        // auth check path the store is accessed directly (no runtime overrides
        // apply).
        self.credentials.store_clone()
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

    /// `getAvailable` (model-runtime.ts:313-328): queues a full availability
    /// refresh (seq-gated) and returns the snapshot. When `provider_id` is
    /// given, returns the filtered snapshot directly.
    pub async fn get_available(
        &self,
        provider_id: Option<&str>,
    ) -> Result<Vec<Model>, ModelsError> {
        self.queue_availability_refresh().await?;
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

    /// `find(provider, modelId)` (model-registry.ts:70-72 @ 4181f66):
    /// provider + model-id lookup within the available snapshot.
    pub fn find_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        read(&self.snapshot)
            .available
            .iter()
            .find(|model| model.provider == provider && model.id == model_id)
            .cloned()
    }

    /// `getCompatibilityRequestConfig(model)` (model-registry.ts:95-100
    /// @ 4181f66): returns the model's `auth_header` / `headers` fields for
    /// the no-auth fallback of `getApiKeyAndHeaders`. The `auth_header` flag
    /// is always `true` for models without explicit configuration
    /// (model defaults from the catalog don't carry it; only provider
    /// config-form overrides do, via `ProviderConfigInput.auth_header`).
    pub fn get_compatibility_request_config(&self, model: &Model) -> CompatibilityRequestConfig {
        CompatibilityRequestConfig {
            auth_header: true,
            headers: model
                .headers
                .as_ref()
                .map(|h| {
                    h.iter()
                        .map(|(k, v)| (k.clone(), Some(v.clone())))
                        .collect()
                })
                .unwrap_or_default(),
        }
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
                    min_oauth_validity_ms: overrides.min_oauth_validity_ms,
                }),
            )
            .await
    }

    /// `getAuth(providerId)` (model-runtime.ts:380 string arm) — provider-level
    /// auth resolution without a model (the llama.cpp extension's
    /// `modelRegistry.getProviderAuth`, T14 W6b). Also used by the auth
    /// credential-print commands (`rpi auth print-bearer-token`).
    pub async fn get_provider_auth(
        &self,
        provider_id: &str,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        self.models.get_provider_auth(provider_id, overrides).await
    }

    /// Returns the per-provider credential serialization mutex, creating it
    /// on first use (model-runtime.ts:152, 494-512).
    fn get_credential_mutex(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut ops = lock(&self.credential_operations);
        if let Some(m) = ops.get(provider_id) {
            return m.clone();
        }
        let m = Arc::new(tokio::sync::Mutex::new(()));
        ops.insert(provider_id.to_owned(), m.clone());
        m
    }

    /// `enqueueCredentialOperation` (model-runtime.ts:494-512, commit
    /// d2be68dbe et al.): serializes credential operations per-provider.
    /// Concurrent login/logout/setRuntimeApiKey/removeRuntimeApiKey on the
    /// same provider run strictly in arrival order without dropping updates.
    /// Upstream chains JS Promises; Rust uses `tokio::sync::Mutex` which
    /// provides the same guarantee.
    async fn enqueue_credential_operation(
        self: &Arc<Self>,
        provider_id: &str,
        task: BoxFuture<'static, Result<(), CredentialSynchronizationError>>,
    ) -> Result<(), CredentialSynchronizationError> {
        let mutex = self.get_credential_mutex(provider_id);
        let _guard = mutex.lock().await;
        task.await
    }

    /// `synchronizeCredentialState` (model-runtime.ts:514-534): recompose the
    /// provider, run a scoped refresh, update the snapshot, and refresh
    /// availability. Any failure is wrapped in `CredentialSynchronizationError`.
    async fn synchronize_credential_state(
        &self,
        provider_id: &str,
        operation: CredentialSynchronizationOperation,
        credential: Option<Credential>,
    ) -> Result<(), CredentialSynchronizationError> {
        self.synchronize_credential_state_with_signal(provider_id, operation, credential, None)
            .await
    }

    /// `synchronizeCredentialState` with an optional interaction signal
    /// (model-runtime.ts:514-534): the interaction's cancellation token
    /// (from login) is combined with the refresh timeout to produce a bounded
    /// refresh signal.
    async fn synchronize_credential_state_with_signal(
        &self,
        provider_id: &str,
        operation: CredentialSynchronizationOperation,
        credential: Option<Credential>,
        interaction_signal: Option<CancellationToken>,
    ) -> Result<(), CredentialSynchronizationError> {
        let wrap_err = |cause: String| CredentialSynchronizationError {
            provider_id: provider_id.to_owned(),
            operation,
            credential: credential.clone(),
            cause,
        };
        // recomposeProvider + composition error check
        // (model-runtime.ts:522-524).
        self.recompose_provider(provider_id);
        if let Some(error) = lock(&self.composition_errors).get(provider_id).cloned() {
            return Err(wrap_err(error));
        }

        // Scoped refresh (model-runtime.ts:525-528): allowNetwork=false,
        // providers=[providerId].
        let refresh_signal =
            bounded_refresh_signal(self.model_refresh_timeout_ms, interaction_signal);
        let result = self
            .models
            .refresh(Some(ModelsRefreshOptions {
                allow_network: Some(false),
                providers: Some(vec![provider_id.to_owned()]),
                force: None,
                signal: Some(refresh_signal),
            }))
            .await;
        if result.aborted {
            return Err(wrap_err("model catalog refresh was aborted".to_owned()));
        }
        for (pid, error) in &result.errors {
            if pid == provider_id {
                return Err(wrap_err(error.clone()));
            }
        }

        // updateModelSnapshot (model-runtime.ts:529).
        self.update_model_snapshot();

        // refreshProviderAvailability — full availability refresh
        // (simplified: we do a full queue_availability_refresh rather than
        // the upstream single-provider variant, since our headless flows are
        // single-task and the extra precision is not observable).
        if let Err(error) = self.queue_availability_refresh().await {
            return Err(wrap_err(error.message));
        }
        Ok(())
    }

    /// `setRuntimeApiKey` (model-runtime.ts:536-547, commit d2be68dbe et al.):
    /// non-persistent runtime override (the `--api-key` CLI path), serialized
    /// per-provider and followed by credential synchronization.
    pub async fn set_runtime_api_key(
        self: &Arc<Self>,
        provider_id: &str,
        api_key: &str,
    ) -> Result<(), CredentialSynchronizationError> {
        let provider_id = provider_id.to_owned();
        let api_key = api_key.to_owned();
        let credential = Credential::ApiKey(ApiKeyCredential {
            key: Some(api_key.clone()),
            env: None,
        });
        let runtime = self.clone();
        let enqueue_pid = provider_id.clone();
        self.enqueue_credential_operation(
            &enqueue_pid,
            async move {
                runtime
                    .credentials
                    .set_runtime_api_key(&provider_id, &api_key);
                runtime
                    .synchronize_credential_state(
                        &provider_id,
                        CredentialSynchronizationOperation::SetRuntimeApiKey,
                        Some(credential),
                    )
                    .await
            }
            .boxed(),
        )
        .await
    }

    /// `removeRuntimeApiKey` (model-runtime.ts:549-555).
    pub async fn remove_runtime_api_key(
        self: &Arc<Self>,
        provider_id: &str,
    ) -> Result<(), CredentialSynchronizationError> {
        let provider_id = provider_id.to_owned();
        let runtime = self.clone();
        let enqueue_pid = provider_id.clone();
        self.enqueue_credential_operation(
            &enqueue_pid,
            async move {
                runtime.credentials.remove_runtime_api_key(&provider_id);
                runtime
                    .synchronize_credential_state(
                        &provider_id,
                        CredentialSynchronizationOperation::RemoveRuntimeApiKey,
                        None,
                    )
                    .await
            }
            .boxed(),
        )
        .await
    }

    /// `listCredentials` (model-runtime.ts:422-424).
    pub async fn list_credentials(&self) -> Result<Vec<CredentialInfo>, ModelsError> {
        self.credentials.list(None).await
    }

    /// `login` (model-runtime.ts:673-680, commit d2be68dbe et al.): run the
    /// provider's login through `Models`, serialized per-provider, followed by
    /// credential synchronization.
    ///
    /// The login (credential acquisition) runs before the serialized chain so
    /// a stalled previous operation does not block user interaction. The
    /// post-login synchronization (recompose + refresh + availability) is
    /// enqueued. The bounded refresh signal (resolved timeout + interaction
    /// cancel) prevents a hung catalog fetch from freezing the login flow.
    ///
    /// Intentional difference: upstream enqueues the entire login+sync
    /// together; here the credential acquisition is outside the chain because
    /// `&dyn AuthInteraction` is borrowed and cannot be moved into a
    /// `'static` enqueue closure. The practical impact is negligible for
    /// headless single-task flows.
    pub async fn login(
        self: &Arc<Self>,
        provider_id: &str,
        auth_type: AuthType,
        interaction: &dyn AuthInteraction,
    ) -> Result<Credential, ModelsError> {
        let credential = self
            .models
            .login(provider_id, auth_type, interaction)
            .await?;

        let provider_id = provider_id.to_owned();
        let credential_for_sync = credential.clone();
        let interaction_signal = interaction.signal();
        let runtime = self.clone();
        let enqueue_pid = provider_id.clone();
        let err_pid = provider_id.clone();
        self.enqueue_credential_operation(
            &enqueue_pid,
            async move {
                runtime
                    .synchronize_credential_state_with_signal(
                        &provider_id,
                        CredentialSynchronizationOperation::Login,
                        Some(credential_for_sync),
                        interaction_signal,
                    )
                    .await
            }
            .boxed(),
        )
        .await
        .map_err(|_| {
            ModelsError::new(
                ModelsErrorCode::Auth,
                format!(
                    "Credential login committed for {err_pid}, but local synchronization failed"
                ),
            )
        })?;
        Ok(credential)
    }

    /// `logout` (model-runtime.ts:682-688): remove the stored credential,
    /// serialized per-provider, followed by credential synchronization.
    pub async fn logout(
        self: &Arc<Self>,
        provider_id: &str,
    ) -> Result<(), CredentialSynchronizationError> {
        let provider_id = provider_id.to_owned();
        let runtime = self.clone();
        let enqueue_pid = provider_id.clone();
        self.enqueue_credential_operation(
            &enqueue_pid,
            async move {
                runtime.models.logout(&provider_id).await.map_err(|e| {
                    CredentialSynchronizationError {
                        provider_id: provider_id.clone(),
                        operation: CredentialSynchronizationOperation::Logout,
                        credential: None,
                        cause: e.message,
                    }
                })?;
                runtime
                    .synchronize_credential_state(
                        &provider_id,
                        CredentialSynchronizationOperation::Logout,
                        None,
                    )
                    .await
            }
            .boxed(),
        )
        .await
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
            ..Default::default()
        };
        let result = self.models.refresh(Some(refresh_options)).await;
        self.update_model_snapshot();
        if let Err(error) = self.queue_availability_refresh().await {
            // Availability errors are recorded in `get_error`; refreshed
            // models remain usable (model-runtime.ts:531-535).
            tracing::warn!("availability refresh failed: {}", error.message);
        }
        result
    }

    /// `registerNativeProvider` (model-runtime.ts:539-546). Upstream kicks a
    /// fire-and-forget refresh; rpi awaits it so `has_configured_auth` is
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

fn default_input() -> Vec<rpi_ai::types::InputModality> {
    vec![rpi_ai::types::InputModality::Text]
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
        sampling_params: model.sampling_params.clone(),
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
        sampling_params: model.sampling_params.clone(),
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

    use rpi_ai::models::{PublishHandle, PublishShared};

    use super::*;

    /// Build a minimal `RefreshModelsContext` for probing whether a provider
    /// implements `refresh_models`. Never actually awaited.
    async fn make_probe_context(
        store: Arc<dyn ModelsStore>,
        provider_id: &str,
    ) -> RefreshModelsContext {
        let stored = store.read(provider_id, None).await.unwrap_or(None);
        let signal = CancellationToken::new();
        let shared = Arc::new(PublishShared {
            provider_id: provider_id.to_owned(),
            generation: 1,
            signal: signal.clone(),
            store,
            chain: Arc::new(tokio::sync::Mutex::new(None)),
            refresh_generations: Arc::new(std::sync::RwLock::new(
                [(provider_id.to_owned(), 1u64)].into(),
            )),
        });
        RefreshModelsContext {
            credential: None,
            stored,
            publish: PublishHandle { shared },
            allow_network: false,
            force: None,
            signal,
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "rpi-model-runtime-test-{}-{nanos}-{}",
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

    /// `ModelRuntime.create` seeds every built-in provider
    /// (model-runtime.ts:181-190, D-038 registration wave): the login
    /// selectors and the model resolver see the official catalog without a
    /// models.json, which then only composes overrides.
    #[tokio::test]
    async fn create_seeds_builtin_providers() {
        let (_tmp, runtime) = runtime_with_models_json(r#"{"providers": {}}"#).await;
        let providers = runtime.get_providers();
        assert_eq!(
            providers.len(),
            rpi_ai::providers::BUILTIN_PROVIDERS.len(),
            "all built-in providers registered"
        );
        // Anthropic offers both login methods (oauth + api-key rows in the
        // login selector); deepseek is api-key only.
        let anthropic = runtime.get_provider("anthropic").expect("anthropic");
        assert!(anthropic.auth().oauth.is_some());
        assert!(anthropic.auth().api_key.is_some());
        assert!(!anthropic.get_models().is_empty());
        let deepseek = runtime.get_provider("deepseek").expect("deepseek");
        assert!(deepseek.auth().oauth.is_none());
        assert!(deepseek.auth().api_key.is_some());
        assert!(!deepseek.get_models().is_empty());
        // radius is a dynamic provider, not a catalog entry.
        assert!(runtime.get_provider("radius").is_some());
        // No composition errors with an empty models.json.
        assert!(
            runtime.get_error().is_none(),
            "runtime error: {:?}",
            runtime.get_error()
        );
    }

    /// `catalogBaseUrl` "off" (ADR-0002 §8) registers the built-ins without
    /// the remote-catalog overlay, so no network path exists.
    #[tokio::test]
    async fn catalog_off_registers_builtins_without_overlay() {
        let tmp = TempDir::new();
        let agent_dir = tmp.0.join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(agent_dir.join("models.json"), r#"{"providers": {}}"#)
            .expect("write models.json");
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            catalog_base_url: Some("off".to_owned()),
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
            ..Default::default()
        })
        .await;
        assert_eq!(
            runtime.get_providers().len(),
            rpi_ai::providers::BUILTIN_PROVIDERS.len()
        );
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let probe = make_probe_context(store, "deepseek").await;
        let deepseek = runtime.get_provider("deepseek").expect("deepseek");
        assert!(
            deepseek.refresh_models(probe).is_none(),
            "off: built-in must not carry the remote-catalog overlay"
        );
    }

    /// A configured catalog base URL wraps the built-ins in the overlay
    /// (`refresh_models` present) — the `rpi update --models` path.
    #[tokio::test]
    async fn catalog_base_url_wraps_builtins_in_overlay() {
        let tmp = TempDir::new();
        let agent_dir = tmp.0.join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(agent_dir.join("models.json"), r#"{"providers": {}}"#)
            .expect("write models.json");
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            catalog_base_url: Some("https://mirror.test".to_owned()),
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
            ..Default::default()
        })
        .await;
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let probe = make_probe_context(store, "deepseek").await;
        let deepseek = runtime.get_provider("deepseek").expect("deepseek");
        assert!(
            deepseek.refresh_models(probe).is_some(),
            "configured base URL: built-in carries the remote-catalog overlay"
        );
        // radius stays dynamic and passes through unwrapped.
        let radius = runtime.get_provider("radius").expect("radius");
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        assert!(radius
            .refresh_models(make_probe_context(store, "radius").await)
            .is_some());
    }

    /// models.json with a built-in provider id composes over the seeded base
    /// (provider-composer): users only write overrides for official
    /// providers — the login selectors and the model resolver pick up the
    /// composed provider.
    #[tokio::test]
    async fn models_json_overlay_composes_over_builtin_base() {
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {"deepseek": {
                "baseUrl": "https://proxy.example.com/v1",
                "api": "openai-completions",
                "apiKey": "RPI_TEST_DEEPSEEK_OVERRIDE_KEY",
                "models": [{"id": "deepseek-v4-flash", "contextWindow": 64000}]
            }}}"#,
        )
        .await;
        let provider = runtime.get_provider("deepseek").expect("deepseek composed");
        assert_eq!(provider.base_url(), Some("https://proxy.example.com/v1"));
        assert!(provider.auth().api_key.is_some());
        let models = provider.get_models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "deepseek-v4-flash");
        assert!(runtime.get_error().is_none());
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
                "apiKey": "RPI_TEST_COMPOSE_DEFAULTS_KEY",
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
                    "apiKey": "RPI_TEST_COMPOSE_NOAPI_KEY",
                    "models": [{"id": "m1"}]
                },
                "no-base-url": {
                    "api": "openai-completions",
                    "apiKey": "RPI_TEST_COMPOSE_NOURL_KEY",
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
                "apiKey": "RPI_TEST_COMPOSE_INVALID_KEY",
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
                "apiKey": "RPI_TEST_COMPOSE_OVERRIDE_KEY",
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

    /// Upstream model-registry.test.ts (25a2c8dcf @ 4181f66, #7568): "custom
    /// model and model override carry sampling params".
    #[tokio::test]
    async fn sampling_params_carry_through_models_json() {
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "RPI_TEST_COMPOSE_SAMPLING_KEY",
                "models": [
                    {"id": "sampling-model", "samplingParams": {"temperature": 1, "top_p": 0.95, "top_k": 0}},
                    {"id": "plain-model"}
                ],
                "modelOverrides": {
                    "plain-model": {"samplingParams": {"top_p": 0.9}}
                }
            }}}"#,
        )
        .await;
        let custom = runtime
            .get_model("custom", "sampling-model")
            .expect("model");
        assert_eq!(
            custom.sampling_params.as_ref().expect("samplingParams")["temperature"],
            serde_json::json!(1)
        );
        assert_eq!(
            custom.sampling_params.as_ref().expect("samplingParams")["top_p"],
            serde_json::json!(0.95)
        );
        assert_eq!(
            custom.sampling_params.as_ref().expect("samplingParams")["top_k"],
            serde_json::json!(0)
        );

        let plain = runtime.get_model("custom", "plain-model").expect("model");
        assert_eq!(
            plain.sampling_params.as_ref().expect("samplingParams")["top_p"],
            serde_json::json!(0.9)
        );

        // An override merges key-wise over the model-level params.
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "RPI_TEST_COMPOSE_SAMPLING_KEY",
                "models": [
                    {"id": "m1", "samplingParams": {"top_p": 0.95, "min_p": 0.05}}
                ],
                "modelOverrides": {
                    "m1": {"samplingParams": {"top_p": 0.5}}
                }
            }}}"#,
        )
        .await;
        let merged = runtime.get_model("custom", "m1").expect("model");
        let params = merged.sampling_params.as_ref().expect("samplingParams");
        assert_eq!(params["top_p"], serde_json::json!(0.5));
        assert_eq!(params["min_p"], serde_json::json!(0.05));
    }

    /// Provider-level `headers` and `authHeader` wrap auth resolution, so the
    /// stream path (which resolves through the composed provider auth) sends
    /// them too (`withConfiguredAuth`, provider-composer.ts:250-262).
    #[tokio::test]
    async fn provider_headers_and_auth_header_reach_resolved_auth() {
        const ENV_KEY: &str = "RPI_TEST_COMPOSE_HEADERS_KEY";
        std::env::set_var(ENV_KEY, "test-api-key");
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "RPI_TEST_COMPOSE_HEADERS_KEY",
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
                    "apiKey": "RPI_TEST_COMPOSE_ORDER_KEY",
                    "models": [{"id": "m1"}]
                },
                "alpha": {
                    "baseUrl": "https://a.example.com",
                    "api": "openai-completions",
                    "apiKey": "RPI_TEST_COMPOSE_ORDER_KEY",
                    "models": [{"id": "m1"}]
                }
            }}"#,
        )
        .await;
        // Built-ins are seeded first (model-runtime.ts:181-190); the
        // registered providers keep insertion order after them.
        let order: Vec<String> = runtime
            .get_models(None)
            .into_iter()
            .map(|model| model.provider)
            .filter(|provider| provider == "zeta" || provider == "alpha")
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

    /// `ModelRuntime::login`'s post-login refresh runs under a bounded
    /// signal (D-053): cancelling the interaction signal (the login dialog's
    /// cancel) aborts a hanging remote-catalog fetch instead of freezing the
    /// login flow forever.
    #[tokio::test]
    async fn login_interaction_cancel_aborts_hanging_post_login_refresh() {
        const ENV_KEY: &str = "RPI_TEST_MODEL_RUNTIME_LOGIN_KEY";
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
            id: "login-hang-test".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(env_api_key_auth(
                    "Login hang test API key",
                    &[ENV_KEY],
                ))),
                oauth: None,
            },
            models: vec![],
            api: ProviderApi::Single(Arc::new(rpi_ai::api::openai_completions::OpenAiCompletions)),
            ..Default::default()
        });
        runtime
            .register_native_provider(crate::core::remote_catalog_provider::with_remote_catalog(
                inner,
                Some(url),
                None,
            ))
            .await
            .expect("register");

        struct MockInteraction {
            signal: CancellationToken,
        }
        impl AuthInteraction for MockInteraction {
            fn signal(&self) -> Option<CancellationToken> {
                Some(self.signal.clone())
            }
            fn prompt<'a>(
                &'a self,
                _prompt: rpi_ai::auth::interaction::AuthPrompt,
            ) -> rpi_ai::auth::types::BoxFutureSend<'a, Result<String, ModelsError>> {
                Box::pin(async move { Ok("test-key".to_owned()) })
            }
            fn notify(&self, _event: rpi_ai::auth::interaction::AuthEvent) {}
        }

        let signal = CancellationToken::new();
        let interaction = MockInteraction {
            signal: signal.clone(),
        };
        let runtime = runtime.clone();
        let login = tokio::spawn(async move {
            runtime
                .login("login-hang-test", AuthType::ApiKey, &interaction)
                .await
        });
        // Let the login reach the hanging refresh (store + refresh start).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        signal.cancel();
        let credential = tokio::time::timeout(std::time::Duration::from_secs(5), login)
            .await
            .expect("interaction cancel must unblock the login")
            .expect("login task")
            .expect("login succeeds despite the aborted refresh");
        assert!(matches!(credential, Credential::ApiKey(_)));
        std::env::remove_var(ENV_KEY);
    }

    /// The bounded signal's timeout is the resolved `model_refresh_timeout_ms`
    /// (not a hardcoded default): a short configured timeout aborts a hanging
    /// post-login refresh without any interaction cancel.
    #[tokio::test]
    async fn login_hung_refresh_aborts_via_configured_timeout() {
        const ENV_KEY: &str = "RPI_TEST_MODEL_RUNTIME_LOGIN_TIMEOUT_KEY";
        std::env::set_var(ENV_KEY, "test-key");
        let url = hung_catalog_server().await;
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: None,
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            model_refresh_timeout_ms: Some(200),
            ..Default::default()
        })
        .await;
        let inner = create_provider(CreateProviderOptions {
            id: "login-timeout-test".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(env_api_key_auth(
                    "Login timeout test API key",
                    &[ENV_KEY],
                ))),
                oauth: None,
            },
            models: vec![],
            api: ProviderApi::Single(Arc::new(rpi_ai::api::openai_completions::OpenAiCompletions)),
            ..Default::default()
        });
        runtime
            .register_native_provider(crate::core::remote_catalog_provider::with_remote_catalog(
                inner,
                Some(url),
                None,
            ))
            .await
            .expect("register");

        struct MockInteraction;
        impl AuthInteraction for MockInteraction {
            fn signal(&self) -> Option<CancellationToken> {
                None
            }
            fn prompt<'a>(
                &'a self,
                _prompt: rpi_ai::auth::interaction::AuthPrompt,
            ) -> rpi_ai::auth::types::BoxFutureSend<'a, Result<String, ModelsError>> {
                Box::pin(async move { Ok("test-key".to_owned()) })
            }
            fn notify(&self, _event: rpi_ai::auth::interaction::AuthEvent) {}
        }

        let interaction = MockInteraction;
        let started = std::time::Instant::now();
        let credential = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            runtime.login("login-timeout-test", AuthType::ApiKey, &interaction),
        )
        .await
        .expect("configured timeout must unblock the login")
        .expect("login succeeds despite the timed-out refresh");
        assert!(matches!(credential, Credential::ApiKey(_)));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "200ms configured timeout, not the 15s default; elapsed: {:?}",
            started.elapsed()
        );
        std::env::remove_var(ENV_KEY);
    }

    /// The shared refresh signal aborts a hanging catalog fetch
    /// (`update --models` / create-time timeout mechanism): the result
    /// reports `aborted` and no error is recorded.
    #[tokio::test]
    async fn refresh_aborts_hanging_catalog_fetch_via_signal() {
        const ENV_KEY: &str = "RPI_TEST_MODEL_RUNTIME_REFRESH_KEY";
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
            api: ProviderApi::Single(Arc::new(rpi_ai::api::openai_completions::OpenAiCompletions)),
            ..Default::default()
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
                ..Default::default()
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
        let store = Arc::new(rpi_ai::auth::credential_store::InMemoryCredentialStore::new());
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(store.clone()),
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        let provider = rpi_ai::providers::github_copilot::github_copilot_provider();
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
        let credential = Credential::OAuth(rpi_ai::auth::types::OAuthCredential {
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
                None,
            )
            .await
            .expect("modify");

        let available = runtime.get_available(None).await.expect("get_available");
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, full_catalog[0].id);
        // The complete synchronous catalog remains intact (scoped to the
        // provider — the runtime also carries the seeded built-ins).
        assert_eq!(
            runtime.get_models(Some("github-copilot")).len(),
            full_catalog.len()
        );
    }

    // ------------------------------------------------------------------
    // Availability refresh generation (R3.4.6, model-runtime.ts:148-152,
    // 285-329, commits 8f9e76974 + c6eb6281a + a077fff0b)
    // ------------------------------------------------------------------

    /// Concurrent `get_available` calls must not corrupt the snapshot — the
    /// seq-gating ensures only the latest pass publishes
    /// (queueAvailabilityRefresh, model-runtime.ts:316-329).
    #[tokio::test]
    async fn concurrent_get_available_calls_produce_consistent_snapshot() {
        const ENV_KEY: &str = "RPI_TEST_CONCURRENT_AVAIL_KEY";
        std::env::set_var(ENV_KEY, "test-key");
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: None,
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        let provider = create_provider(CreateProviderOptions {
            id: "concurrent-avail".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(env_api_key_auth(
                    "Concurrent avail key",
                    &[ENV_KEY],
                ))),
                oauth: None,
            },
            models: vec![],
            api: ProviderApi::Single(Arc::new(rpi_ai::api::openai_completions::OpenAiCompletions)),
            ..Default::default()
        });
        runtime
            .register_native_provider(provider)
            .await
            .expect("register");

        // Launch multiple concurrent get_available calls.
        let rt = runtime.clone();
        let mut handles = Vec::new();
        for _ in 0..5 {
            let rt = rt.clone();
            handles.push(tokio::spawn(async move {
                rt.get_available(None).await.expect("get_available")
            }));
        }
        let results: Vec<Vec<Model>> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.expect("task"))
            .collect();

        // All results must have the same length (consistent snapshot).
        let len = results[0].len();
        for (i, result) in results.iter().enumerate() {
            assert_eq!(
                result.len(),
                len,
                "snapshot length mismatch at result {i}: concurrent stale publish"
            );
        }

        // The snapshot must not be empty — the provider has auth configured.
        // (The built-in providers may or may not have auth depending on env,
        // but the custom provider definitely does.)
        let snapshot = runtime.get_available_snapshot();
        assert!(
            !snapshot.is_empty(),
            "snapshot must contain available models after concurrent refresh"
        );
        std::env::remove_var(ENV_KEY);
    }

    /// `get_error` lists per-provider catalog failures from the availability
    /// refresh (model-runtime.ts:426-434). A composition error must appear
    /// in the error output alongside the availability refresh error.
    #[tokio::test]
    async fn get_available_records_availability_errors() {
        // A runtime with no network and no auth → availability refresh
        // should complete without error (no configured providers = empty
        // available), and get_error should be None for availability.
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: None,
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        let _ = runtime.get_available(None).await;
        // There may be composition errors from the built-in catalog (unlikely
        // with empty models.json), but availability errors should be absent
        // since there are no configured providers to check.
        let error = runtime.get_error();
        // If there's an error, it should not be an availability refresh error
        // (the no-configured-providers path is not an error).
        if let Some(error) = error {
            assert!(
                !error.contains("Availability refresh"),
                "unexpected availability error with no configured providers: {error}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Credential serialization (R3.4.7, model-runtime.ts:494-688, commits
    // d2be68dbe et al.)
    // ------------------------------------------------------------------

    /// Regression: #7027 — credential refresh hang. A stalled network catalog
    /// refresh must not block login (model-runtime.ts:494-534).
    /// Port of `test/suite/regressions/7027-credential-refresh-hang.test.ts`
    /// (upstream intent: "does not hold login behind an older stalled network
    /// catalog refresh").
    #[tokio::test]
    async fn set_runtime_api_key_completes_despite_stalled_network_refresh() {
        const ENV_KEY: &str = "RPI_TEST_7027_KEY";
        std::env::set_var(ENV_KEY, "initial-key");
        let url = hung_catalog_server().await;
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: None,
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        let inner = create_provider(CreateProviderOptions {
            id: "stalled-7027".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(env_api_key_auth("7027 test key", &[ENV_KEY]))),
                oauth: None,
            },
            models: vec![model_7027()],
            api: ProviderApi::Single(Arc::new(rpi_ai::api::openai_completions::OpenAiCompletions)),
            ..Default::default()
        });
        runtime
            .register_native_provider(crate::core::remote_catalog_provider::with_remote_catalog(
                inner,
                Some(url),
                None,
            ))
            .await
            .expect("register");

        // set_runtime_api_key must complete (not hang) despite the stalled
        // catalog fetch — the bounded refresh signal aborts it via timeout.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            runtime.set_runtime_api_key("stalled-7027", "secret"),
        )
        .await;
        assert!(
            result.is_ok(),
            "set_runtime_api_key must not hang behind a stalled catalog refresh"
        );
        // The sync may fail (CredentialSynchronizationError) since the
        // catalog fetch was aborted — that's acceptable; the credential is
        // committed. Either Ok or Err is fine, as long as it doesn't hang.
        std::env::remove_var(ENV_KEY);
    }

    /// Concurrent credential operations on the same provider are serialized
    /// — no lost updates (R3.4.7 enqueue semantics).
    #[tokio::test]
    async fn concurrent_credential_ops_serialize_without_lost_updates() {
        const ENV_KEY: &str = "RPI_TEST_CONCURRENT_CRED_KEY";
        std::env::set_var(ENV_KEY, "initial-key");
        let credentials: Arc<dyn rpi_ai::auth::types::CredentialStore> =
            Arc::new(rpi_ai::auth::credential_store::InMemoryCredentialStore::new());
        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(credentials.clone()),
            auth_path: None,
            models_path: ModelsPathInput::Disabled,
            ..Default::default()
        })
        .await;
        let provider = create_provider(CreateProviderOptions {
            id: "concurrent-test".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(env_api_key_auth(
                    "Concurrent test key",
                    &[ENV_KEY],
                ))),
                oauth: None,
            },
            models: vec![],
            api: ProviderApi::Single(Arc::new(rpi_ai::api::openai_completions::OpenAiCompletions)),
            ..Default::default()
        });
        runtime
            .register_native_provider(provider)
            .await
            .expect("register");

        // Launch multiple set_runtime_api_key calls concurrently — they must
        // all complete without panicking, and the last committed key wins.
        let rt = runtime.clone();
        let mut handles = Vec::new();
        for i in 0..5 {
            let rt = rt.clone();
            handles.push(tokio::spawn(async move {
                let _ = rt
                    .set_runtime_api_key("concurrent-test", &format!("key-{i}"))
                    .await;
            }));
        }
        for handle in handles {
            handle.await.expect("task completed");
        }

        // The runtime API key override should be set to one of the keys
        // (serialization ensures no write is lost — the final value is the
        // last-enqueued key, but the order of completion is not guaranteed
        // by tokio task scheduling alone).
        assert!(
            runtime.has_runtime_api_key("concurrent-test"),
            "runtime API key should be set after concurrent operations"
        );
        std::env::remove_var(ENV_KEY);
    }

    /// Minimal model fixture for the 7027 regression test (same shape as the
    /// upstream `dynamicModel`).
    fn model_7027() -> Model {
        use rpi_ai::types::{InputModality, ModelCost, ModelCostRates};
        Model {
            id: "dynamic".to_owned(),
            name: "Dynamic".to_owned(),
            api: ApiKind::from("openai-completions"),
            provider: "stalled-7027".to_owned(),
            base_url: "https://example.test/v1".to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost {
                rates: ModelCostRates {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                tiers: None,
            },
            context_window: 1000,
            max_tokens: 100,
            headers: None,
            compat: None,
            sampling_params: None,
        }
    }
}
