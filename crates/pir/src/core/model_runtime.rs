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
//! - `refresh` never touches the network (remote model catalogs are T13); it
//!   reloads models.json and rebuilds providers.
//! - models.json `apiKey` is treated as an env var name
//!   ([`env_api_key_auth`]); command/raw-key config values
//!   (resolve-config-value.ts) and `oauth: "radius"` are T13.
//! - Per-model `api` overrides inside one provider are not honored (the
//!   provider-level `api` streams all models).
//! - Availability is recomputed on each `get_available` call (upstream
//!   coalesces concurrent refreshes onto one in-flight promise — the
//!   single-threaded headless flows cannot observe the difference).
//!
//! Upstream mutates through plain class fields; here the mutable registries
//! live behind mutexes so all methods take `&self` (JS has no borrow
//! discipline — this is structural, not behavioral).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use pir_ai::api::anthropic_messages::AnthropicMessages;
use pir_ai::api::openai_completions::OpenAiCompletions;
use pir_ai::api::openai_responses::OpenAiResponses;
use pir_ai::auth::file_store::FileCredentialStore;
use pir_ai::auth::helpers::env_api_key_auth;
use pir_ai::auth::resolve::{
    resolve_provider_auth, AuthResolutionOverrides, ModelsError, ModelsErrorCode,
};
use pir_ai::auth::types::{
    ApiKeyCredential, AuthCheck, AuthContext, AuthResult, AuthType, Credential, CredentialInfo,
    CredentialStore, CredentialType, DefaultAuthContext, ModifyFn, ProviderAuth,
};
use pir_ai::models::{
    create_provider, CreateModelsOptions, CreateProviderOptions, Models, ModelsSimpleStreamOptions,
    ModelsStreamOptions, Provider, ProviderApi, ProviderStreams,
};
use pir_ai::models_json::{ModelConfig, ModelsJsonModel, ModelsJsonProvider};
use pir_ai::types::{ApiKind, AssistantMessage, Context, Model, ProviderEnv, ProviderHeaders};
use pir_ai::utils::event_stream::AssistantMessageEventStream;

use crate::config::get_agent_dir;

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
#[derive(Debug, Clone, Default)]
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
/// closure-bearing fields (`streamSimple`, `oauth`, `refreshModels`) arrive
/// with the extension host (T15).
#[derive(Debug, Clone, Default)]
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

/// `CreateModelRuntimeOptions` (model-runtime.ts:58-70), T10 subset (no
/// network refresh options — see module docs).
#[derive(Default)]
pub struct CreateModelRuntimeOptions {
    /// Credential storage. Default: file at `auth_path`.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    /// Default: `{agentDir}/auth.json`.
    pub auth_path: Option<PathBuf>,
    pub models_path: ModelsPathInput,
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
    native_providers: Mutex<HashMap<String, Arc<dyn Provider>>>,
    extension_providers: Mutex<HashMap<String, ProviderConfigInput>>,
    composition_errors: Mutex<HashMap<String, String>>,
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
        let models = Models::new(Some(CreateModelsOptions {
            credentials: Some(credentials.clone()),
            auth_context: None,
        }));

        let runtime = Arc::new(ModelRuntime {
            models,
            credentials,
            models_path,
            config: Mutex::new(config),
            native_providers: Mutex::new(HashMap::new()),
            extension_providers: Mutex::new(HashMap::new()),
            composition_errors: Mutex::new(HashMap::new()),
            snapshot: RwLock::new(ModelRuntimeSnapshot::default()),
        });
        runtime.rebuild_providers();
        // create() always refreshes availability; upstream awaits refresh here
        // too (allowNetwork: false in the T10 subset — see module docs).
        if let Err(error) = runtime.refresh_availability().await {
            tracing::warn!("availability refresh failed: {}", error.message);
        }
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
        let auth = match &api_key_value {
            Some(env_var) => ProviderAuth {
                api_key: Some(Arc::new(env_api_key_auth(
                    format!("{provider_id} API key"),
                    &[env_var.as_str()],
                ))),
                oauth: None,
            },
            None => base.map(|b| b.auth().clone()).ok_or_else(|| {
                format!("Provider {provider_id}: no authentication method configured.")
            })?,
        };

        let api = extension
            .and_then(|e| e.api.clone())
            .or_else(|| config.and_then(|c| c.api.clone()));
        let streams = match (&api, base) {
            (Some(api), _) => api_streams(api)
                .ok_or_else(|| format!("No API provider registered for api: {api}"))?,
            (None, Some(base)) => {
                // Overlay without its own api: stream through the base
                // provider untouched (models/auth passthrough).
                return Ok(base.clone());
            }
            (None, None) => {
                return Err(format!("Provider {provider_id}: no api configured."));
            }
        };

        let models = extension
            .and_then(|e| e.models.clone())
            .map(|models| {
                models
                    .into_iter()
                    .map(|m| config_model_to_model(provider_id, &api, base_url.as_deref(), m))
                    .collect::<Vec<_>>()
            })
            .or_else(|| {
                config.and_then(|c| c.models.clone()).map(|models| {
                    models
                        .into_iter()
                        .map(|m| json_model_to_model(provider_id, &api, base_url.as_deref(), m))
                        .collect::<Vec<_>>()
                })
            })
            .or_else(|| base.map(|b| b.get_models()))
            .unwrap_or_default();

        let headers = base.and_then(|b| b.headers().cloned());

        Ok(create_provider(CreateProviderOptions {
            id: provider_id.to_owned(),
            name: Some(name),
            base_url,
            headers,
            auth,
            models,
            api: ProviderApi::Single(streams),
        }))
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

    /// `runAvailabilityRefresh` (model-runtime.ts:240-268).
    async fn refresh_availability(&self) -> Result<(), ModelsError> {
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
        let available = all
            .iter()
            .filter(|model| configured.contains(&model.provider))
            .cloned()
            .collect();

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

    /// Configured provider-level headers from models.json / extension
    /// registration (`configuredHeaders`, provider-composer.ts subset).
    fn configured_provider_headers(&self, provider_id: &str) -> Option<ProviderHeaders> {
        let config_headers = lock(&self.config)
            .get_provider(provider_id)
            .and_then(|c| c.headers.as_ref().map(strings_to_headers));
        let extension_headers = lock(&self.extension_providers)
            .get(provider_id)
            .and_then(|e| e.headers.as_ref().map(strings_to_headers));
        merge_headers(config_headers.as_ref(), extension_headers.as_ref())
    }

    /// `getAuth(model, overrides)` (model-runtime.ts:374-396).
    pub async fn get_auth(
        &self,
        model: &Model,
        overrides: Option<&ModelRuntimeAuthOverrides>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        let overrides = overrides.cloned().unwrap_or_default();
        let resolution = self
            .models
            .get_auth(
                model,
                Some(&AuthResolutionOverrides {
                    api_key: overrides.api_key.clone(),
                    env: overrides.env.clone(),
                }),
            )
            .await?;
        let Some(mut resolution) = resolution else {
            return Ok(None);
        };
        let configured = self.configured_provider_headers(&model.provider);
        resolution.auth.headers =
            merge_headers(resolution.auth.headers.as_ref(), configured.as_ref());
        Ok(Some(resolution))
    }

    /// `setRuntimeApiKey` (model-runtime.ts:398-415) — non-persistent
    /// runtime override (the `--api-key` CLI path).
    pub async fn set_runtime_api_key(&self, provider_id: &str, api_key: &str) {
        self.credentials.set_runtime_api_key(provider_id, api_key);
        {
            let mut snapshot = write(&self.snapshot);
            snapshot.auth.insert(
                provider_id.to_owned(),
                AuthCheck {
                    source: Some("runtime API key".to_owned()),
                    kind: AuthType::ApiKey,
                },
            );
            snapshot.configured_providers.insert(provider_id.to_owned());
            snapshot.stored_providers.insert(provider_id.to_owned());
            let configured = snapshot.configured_providers.clone();
            snapshot.available = snapshot
                .all
                .iter()
                .filter(|model| configured.contains(&model.provider))
                .cloned()
                .collect();
        }
        if let Err(error) = self.refresh_availability().await {
            tracing::warn!("availability refresh failed: {}", error.message);
        }
    }

    /// `removeRuntimeApiKey` (model-runtime.ts:417-420).
    pub async fn remove_runtime_api_key(&self, provider_id: &str) {
        self.credentials.remove_runtime_api_key(provider_id);
        if let Err(error) = self.refresh_availability().await {
            tracing::warn!("availability refresh failed: {}", error.message);
        }
    }

    /// `listCredentials` (model-runtime.ts:422-424).
    pub async fn list_credentials(&self) -> Result<Vec<CredentialInfo>, ModelsError> {
        self.credentials.list().await
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

    /// `refresh` (model-runtime.ts:516-537): reload models.json, rebuild
    /// providers, recompute availability. Never touches the network (T13).
    pub async fn refresh(&self) {
        let config = ModelConfig::load(self.models_path.as_deref()).await;
        *lock(&self.config) = config;
        self.rebuild_providers();
        if let Err(error) = self.refresh_availability().await {
            tracing::warn!("availability refresh failed: {}", error.message);
        }
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
        if let Err(error) = self.refresh_availability().await {
            tracing::warn!("availability refresh failed: {}", error.message);
        }
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
        if let Err(error) = self.refresh_availability().await {
            tracing::warn!("availability refresh failed: {}", error.message);
        }
        Ok(())
    }

    /// `unregisterProvider` (model-runtime.ts:586-592).
    pub async fn unregister_provider(&self, provider_id: &str) {
        lock(&self.extension_providers).remove(provider_id);
        lock(&self.native_providers).remove(provider_id);
        self.recompose_provider(provider_id);
        self.update_model_snapshot();
        if let Err(error) = self.refresh_availability().await {
            tracing::warn!("availability refresh failed: {}", error.message);
        }
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

/// models.json model definition → runtime `Model`.
fn json_model_to_model(
    provider_id: &str,
    provider_api: &Option<String>,
    provider_base_url: Option<&str>,
    model: ModelsJsonModel,
) -> Model {
    Model {
        name: model.name.clone().unwrap_or_else(|| model.id.clone()),
        api: ApiKind::from(
            model
                .api
                .clone()
                .or_else(|| provider_api.clone())
                .unwrap_or_default(),
        ),
        provider: provider_id.to_owned(),
        base_url: model
            .base_url
            .clone()
            .or_else(|| provider_base_url.map(str::to_owned))
            .unwrap_or_default(),
        reasoning: model.reasoning.unwrap_or(false),
        thinking_level_map: model.thinking_level_map.clone(),
        input: model.input.clone().unwrap_or_else(default_input),
        cost: model.cost.clone().unwrap_or_default(),
        context_window: model.context_window.unwrap_or(0.0) as u32,
        max_tokens: model.max_tokens.unwrap_or(0.0) as u32,
        headers: model
            .headers
            .as_ref()
            .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        id: model.id,
        compat: model.compat.clone(),
    }
}

/// Extension `ProviderConfigModel` → runtime `Model`.
fn config_model_to_model(
    provider_id: &str,
    provider_api: &Option<String>,
    provider_base_url: Option<&str>,
    model: ProviderConfigModel,
) -> Model {
    Model {
        name: model.name.clone().unwrap_or_else(|| model.id.clone()),
        api: ApiKind::from(
            model
                .api
                .clone()
                .or_else(|| provider_api.clone())
                .unwrap_or_default(),
        ),
        provider: provider_id.to_owned(),
        base_url: model
            .base_url
            .clone()
            .or_else(|| provider_base_url.map(str::to_owned))
            .unwrap_or_default(),
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
    }
}
