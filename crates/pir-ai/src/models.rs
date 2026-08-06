//! Port of `packages/ai/src/models.ts` @ pi 0.82.1 (2efa728) — T03 scope:
//! `Provider` / `ProviderStreams` traits, `create_provider` with api-map
//! dispatch, `Models` (auth application + stream/stream_simple dispatch),
//! thinking-level helpers.
//!
//! Out of T03 scope (upstream features landing later): `checkAuth` /
//! `getAvailable` / `login` / `logout` (T04). `filterModels` landed in T13 W4
//! as a [`Provider`] trait method; the availability surface that applies it
//! (`get_available`, model-runtime.ts:240-268 → models.ts:394-408) lives in
//! pir's model runtime (`crates/pir/src/core/model_runtime.rs`), not on
//! `Models`. The dynamic provider overlay (`refreshModels` /
//! `inflightRefresh` dedup, models.ts:276-328) landed in T13 W6-C:
//! [`Models::refresh`], [`Provider::refresh_models`], [`InflightRefresh`]
//! and the [`crate::models_store`] plumbing. `CreateProviderOptions.
//! fetchModels` is deferred to the provider-composer extension refresh
//! wiring (T15; see D-036) — the refresh pattern lives in the radius and
//! remote-catalog providers instead.
//!
//! Intentional differences:
//! - `ApiStreamOptions<TApi>` per-API extras collapse to `StreamOptions`
//!   (Azure extras arrive with the Azure adapter, T13).
//! - The trait for API implementations is named `ProviderStreams` (upstream
//!   name); design §3.3 calls it `ApiStream` — same concept.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use crate::auth::{
    resolve_provider_auth, AuthContext, AuthResolutionOverrides, AuthResult, Credential,
    CredentialStore, DefaultAuthContext, InMemoryCredentialStore, ModelsError, ModelsErrorCode,
    ProviderAuth,
};
use crate::models_json::OrderedMap;
use crate::models_store::{
    InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ProviderModelsStore,
};
use crate::types::{
    Context, Model, ModelThinkingLevel, ProviderHeaders, SimpleStreamOptions, StreamOptions,
};
use crate::utils::event_stream::AssistantMessageEventStream;
use crate::utils::headers::merge_headers;

use super::api::lazy::lazy_stream;
use super::error::AiError;

/// `ProviderStreams` — the API-adapter interface (design §3.3 `ApiStream`).
/// Implementations return the event stream synchronously; all failures are
/// encoded as `StreamEvent::Error` on the stream (never `Err`).
pub trait ProviderStreams: Send + Sync {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream;

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream;
}

/// `transformHeaders` callback (models.ts `ModelsStreamTransforms`).
pub type TransformHeadersCallback = Arc<
    dyn Fn(ProviderHeaders) -> futures::future::BoxFuture<'static, ProviderHeaders> + Send + Sync,
>;

/// `ModelsApiStreamOptions` = `StreamOptions` + `transformHeaders`.
#[derive(Clone, Default)]
pub struct ModelsStreamOptions {
    pub stream: StreamOptions,
    pub transform_headers: Option<TransformHeadersCallback>,
}

impl From<StreamOptions> for ModelsStreamOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            transform_headers: None,
        }
    }
}

/// `ModelsSimpleStreamOptions` = `SimpleStreamOptions` + `transformHeaders`.
#[derive(Clone, Default)]
pub struct ModelsSimpleStreamOptions {
    pub simple: SimpleStreamOptions,
    pub transform_headers: Option<TransformHeadersCallback>,
}

impl From<SimpleStreamOptions> for ModelsSimpleStreamOptions {
    fn from(simple: SimpleStreamOptions) -> Self {
        Self {
            simple,
            transform_headers: None,
        }
    }
}

/// `Provider` — the concrete runtime unit (T03 subset, see module docs).
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn base_url(&self) -> Option<&str>;
    fn headers(&self) -> Option<&ProviderHeaders>;
    fn auth(&self) -> &ProviderAuth;

    /// Current known models, sync. Must not panic; `Models` treats a panicking
    /// implementation as having no models.
    fn get_models(&self) -> Vec<Model>;

    /// `filterModels` — optional provider policy for credential-specific
    /// model availability (models.ts:111). `get_models` remains the complete
    /// synchronous catalog; the model runtime's availability refresh
    /// (`crates/pir/src/core/model_runtime.rs` `refresh_availability_inner`)
    /// applies this filter after confirming that provider auth is
    /// configured. Default: no filter.
    fn filter_models(&self, models: Vec<Model>, credential: Option<&Credential>) -> Vec<Model> {
        let _ = credential;
        models
    }

    /// `refreshModels?` (models.ts:104): restore the provider-scoped stored
    /// catalog and optionally fetch a newer list using the effective
    /// credential. Implementations must retain their previous list on failure
    /// and honor the shared abort signal for network requests. `None` = no
    /// dynamic overlay (upstream: the method is absent; [`Models::refresh`]
    /// skips such providers before resolving credentials).
    fn refresh_models(
        &self,
        _context: RefreshModelsContext,
    ) -> Option<BoxFuture<'_, Result<(), ModelsError>>> {
        None
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream;

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream;
}

/// `RefreshModelsContext` (models.ts:34-44).
#[derive(Clone)]
pub struct RefreshModelsContext {
    /// Effective configured credential; OAuth credentials are refreshed
    /// before network access.
    pub credential: Option<Credential>,
    /// Persistent model storage scoped to this provider ID.
    pub store: Arc<dyn ProviderModelsStore>,
    /// False during offline/cache-only initialization.
    pub allow_network: bool,
    /// Bypass provider freshness checks and fetch immediately when network
    /// access is allowed.
    pub force: bool,
    pub signal: Option<CancellationToken>,
}

/// `ModelsRefreshOptions` (models.ts:46-51).
#[derive(Clone, Default)]
pub struct ModelsRefreshOptions {
    /// Defaults to `true` (models.ts:277 `options.allowNetwork ?? true`).
    pub allow_network: Option<bool>,
    /// Bypass provider freshness checks and fetch immediately when network
    /// access is allowed.
    pub force: Option<bool>,
    pub signal: Option<CancellationToken>,
}

/// `ModelsRefreshResult` (models.ts:53-56). Provider errors are returned
/// without rejecting; provider ids keep insertion order (like the upstream
/// `Map` iteration the CLI joins).
#[derive(Debug, Default)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    /// Provider id → error message, in provider iteration order.
    pub errors: Vec<(String, String)>,
}

/// In-flight refresh future shared by concurrent callers.
pub type InflightRefreshFuture =
    futures::future::Shared<BoxFuture<'static, Result<(), ModelsError>>>;

/// `inflightRefresh` dedup slot (models.ts:559, remote-catalog-provider.ts:50,
/// radius.ts:25): the first caller runs the refresh; concurrent callers await
/// the same in-flight future; a completed refresh is dropped from the slot so
/// the next call starts fresh (upstream `inflightRefresh ??= …` +
/// `finally { inflightRefresh = undefined }`).
pub struct InflightRefresh {
    slot: Mutex<Option<InflightRefreshFuture>>,
}

impl Default for InflightRefresh {
    fn default() -> Self {
        Self::new()
    }
}

impl InflightRefresh {
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    /// Run `refresh` unless one is already in flight; concurrent callers join
    /// it and observe its result. Locks are held only around the take/replace
    /// of the slot (no `.await` under the lock, coding-standards §6.5).
    pub async fn join_or_run<F>(&self, refresh: F) -> Result<(), ModelsError>
    where
        F: Future<Output = Result<(), ModelsError>> + Send + 'static,
    {
        let (shared, runner) = {
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            match slot.take() {
                Some(shared) => {
                    // Already in flight: re-insert and join the same future.
                    *slot = Some(shared.clone());
                    (shared, false)
                }
                None => {
                    let shared = refresh.boxed().shared();
                    *slot = Some(shared.clone());
                    (shared, true)
                }
            }
        };
        let result = shared.await;
        if runner {
            // The in-flight refresh finished; drop it so a later call starts
            // fresh (upstream `finally { inflightRefresh = undefined }`).
            self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        result
    }
}

/// `mergeModels` (remote-catalog-provider.ts:8-16): the dynamic overlay over
/// the baseline — same-id models replace the baseline entry, new ids append.
pub fn merge_models(baseline: &[Model], dynamic: &[Model]) -> Vec<Model> {
    let mut merged: Vec<Model> = baseline.to_vec();
    for model in dynamic {
        match merged.iter_mut().find(|entry| entry.id == model.id) {
            Some(entry) => *entry = model.clone(),
            None => merged.push(model.clone()),
        }
    }
    merged
}

/// Unix epoch milliseconds (`Date.now()`).
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Store errors surface as `ModelsError("model_source", …)` on the refresh
/// path (models.ts wraps non-`Error` rejections the same way).
pub(crate) fn ai_error_to_models_error(error: AiError) -> ModelsError {
    ModelsError::new(ModelsErrorCode::ModelSource, error.to_string())
}

/// `createProvider` api input: a single implementation for all models, or a
/// map keyed by `model.api` for mixed-API providers.
#[derive(Clone)]
pub enum ProviderApi {
    Single(Arc<dyn ProviderStreams>),
    Map(HashMap<String, Arc<dyn ProviderStreams>>),
}

/// `CreateProviderOptions`.
pub struct CreateProviderOptions {
    pub id: String,
    /// Display name. Default: `id`.
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub headers: Option<ProviderHeaders>,
    /// Required — every provider has auth semantics.
    pub auth: ProviderAuth,
    /// Static baseline model list.
    pub models: Vec<Model>,
    pub api: ProviderApi,
}

struct CreatedProvider {
    id: String,
    name: String,
    base_url: Option<String>,
    headers: Option<ProviderHeaders>,
    auth: ProviderAuth,
    models: Vec<Model>,
    api: ProviderApi,
}

impl CreatedProvider {
    fn api_for(&self, model: &Model) -> Option<&Arc<dyn ProviderStreams>> {
        match &self.api {
            ProviderApi::Single(streams) => Some(streams),
            ProviderApi::Map(by_api) => by_api.get(model.api.as_str()),
        }
    }

    fn dispatch(
        &self,
        model: &Model,
        run: impl FnOnce(&Arc<dyn ProviderStreams>) -> AssistantMessageEventStream,
    ) -> AssistantMessageEventStream {
        match self.api_for(model) {
            Some(streams) => run(streams),
            None => {
                let id = self.id.clone();
                let api = model.api.clone();
                lazy_stream(model, async move {
                    Err(ModelsError::new(
                        ModelsErrorCode::Stream,
                        format!("Provider {id} has no API implementation for \"{api}\""),
                    ))
                })
            }
        }
    }
}

impl Provider for CreatedProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    fn headers(&self) -> Option<&ProviderHeaders> {
        self.headers.as_ref()
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.dispatch(model, |streams| streams.stream(model, context, options))
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.dispatch(model, |streams| {
            streams.stream_simple(model, context, options)
        })
    }
}

/// `createProvider`: builds a provider from parts. A single `api` streams all
/// models; an api map dispatches on `model.api`, and a model whose api has no
/// entry produces a stream error.
pub fn create_provider(input: CreateProviderOptions) -> Arc<dyn Provider> {
    Arc::new(CreatedProvider {
        name: input.name.unwrap_or_else(|| input.id.clone()),
        id: input.id,
        base_url: input.base_url,
        headers: input.headers,
        auth: input.auth,
        models: input.models,
        api: input.api,
    })
}

/// `CreateModelsOptions`.
pub struct CreateModelsOptions {
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub auth_context: Option<Arc<dyn AuthContext>>,
    /// Persistent model storage for dynamic provider overlays
    /// (`modelsStore`, models.ts:198-226). Defaults to in-memory.
    pub models_store: Option<Arc<dyn ModelsStore>>,
}

/// `Models` — runtime collection of providers plus auth application and
/// stream convenience (T03 subset, see module docs).
#[derive(Clone)]
pub struct Models {
    /// Insertion-ordered like the upstream JS `Map`: `getModels()` order is
    /// observable (initial-model fallback, available-model listings).
    providers: Arc<RwLock<OrderedMap<Arc<dyn Provider>>>>,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
    models_store: Arc<dyn ModelsStore>,
}

impl Default for Models {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Models {
    /// `createModels`.
    pub fn new(options: Option<CreateModelsOptions>) -> Self {
        let options = options.unwrap_or(CreateModelsOptions {
            credentials: None,
            auth_context: None,
            models_store: None,
        });
        Self {
            providers: Arc::new(RwLock::new(OrderedMap::default())),
            credentials: options
                .credentials
                .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new())),
            auth_context: options
                .auth_context
                .unwrap_or_else(|| Arc::new(DefaultAuthContext)),
            models_store: options
                .models_store
                .unwrap_or_else(|| Arc::new(InMemoryModelsStore::new())),
        }
    }

    /// `setProvider` — upsert/replace by `provider.id`.
    pub fn set_provider(&self, provider: Arc<dyn Provider>) {
        self.providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(provider.id().to_owned(), provider);
    }

    pub fn delete_provider(&self, id: &str) {
        self.providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    pub fn clear_providers(&self) {
        self.providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn get_providers(&self) -> Vec<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn get_provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// `getModels` — sync read of last-known models from one provider or all.
    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        let providers = self.providers.read().unwrap_or_else(|e| e.into_inner());
        match provider {
            Some(id) => providers
                .get(id)
                .map(|provider| provider.get_models())
                .unwrap_or_default(),
            None => providers
                .values()
                .flat_map(|provider| provider.get_models())
                .collect(),
        }
    }

    /// `getModel` — sync runtime model lookup.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_models(Some(provider))
            .into_iter()
            .find(|model| model.id == id)
    }

    /// `refresh(options)` (models.ts:276-328): refresh every configured
    /// dynamic provider concurrently. Provider errors and cancellation are
    /// returned without rejecting; static and unconfigured providers are
    /// skipped. On error the stored overlay is restored with
    /// `allowNetwork: false` (offline recovery), preserving the cached
    /// catalog over the original error.
    pub async fn refresh(&self, options: Option<ModelsRefreshOptions>) -> ModelsRefreshResult {
        let options = options.unwrap_or_default();
        let allow_network = options.allow_network.unwrap_or(true);
        let signal = options.signal.clone();

        let providers = self.get_providers();
        let futures = providers.into_iter().map(|provider| {
            let this = self.clone();
            let options = options.clone();
            let signal = signal.clone();
            async move {
                let provider_id = provider.id().to_owned();
                let store: Arc<dyn ProviderModelsStore> = Arc::new(ScopedModelsStore {
                    store: this.models_store.clone(),
                    provider_id: provider_id.clone(),
                });
                // Upstream filters on `refreshModels !== undefined` before
                // any credential work (models.ts:279-283); the probe never
                // polls the returned future, so it is side-effect-free.
                let probe = RefreshModelsContext {
                    credential: None,
                    store: store.clone(),
                    allow_network: false,
                    force: false,
                    signal: None,
                };
                // The probe future is dropped un-polled on purpose (see
                // above); the `?` skips non-refreshable providers.
                std::mem::drop(provider.refresh_models(probe)?);
                let outcome: Result<(), ModelsError> = async {
                    let stored = this.read_credential(&provider_id).await?;
                    let credential = this
                        .resolve_refresh_credential(
                            &provider,
                            stored.clone(),
                            allow_network,
                            signal.as_ref(),
                        )
                        .await?;
                    let Some(credential) = credential else {
                        // Unconfigured provider: nothing to refresh
                        // (models.ts:296-298).
                        return Ok(());
                    };
                    let context = RefreshModelsContext {
                        credential: Some(credential),
                        store: store.clone(),
                        allow_network,
                        force: options.force.unwrap_or(false),
                        signal: signal.clone(),
                    };
                    let Some(refresh) = provider.refresh_models(context) else {
                        return Ok(());
                    };
                    refresh.await
                }
                .await;

                match outcome {
                    Ok(()) => None,
                    Err(error) => {
                        // Cancellation is reported via `aborted`, not the
                        // error map (models.ts:305).
                        if signal.as_ref().is_some_and(|t| t.is_cancelled()) {
                            return None;
                        }
                        // Cache restoration is best-effort; the original
                        // error is preserved (models.ts:313-322).
                        this.restore_cached_overlay(&provider, store, signal.as_ref())
                            .await;
                        Some((provider_id, error.message))
                    }
                }
            }
        });
        let results: Vec<Option<(String, String)>> = futures::future::join_all(futures).await;

        ModelsRefreshResult {
            aborted: signal.as_ref().is_some_and(|t| t.is_cancelled()),
            errors: results.into_iter().flatten().collect(),
        }
    }

    /// `readCredential` (models.ts:356-362).
    async fn read_credential(&self, provider_id: &str) -> Result<Option<Credential>, ModelsError> {
        self.credentials.read(provider_id).await.map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::Auth,
                format!("Credential store read failed for {provider_id}"),
                &error.message,
            )
        })
    }

    /// `resolveRefreshCredential` (models.ts:330-354): refresh expired OAuth
    /// tokens under the store lock (serialized by `CredentialStore::modify`),
    /// or resolve api-key auth into a stored-shaped credential.
    async fn resolve_refresh_credential(
        &self,
        provider: &Arc<dyn Provider>,
        stored: Option<Credential>,
        allow_network: bool,
        signal: Option<&CancellationToken>,
    ) -> Result<Option<Credential>, ModelsError> {
        if let Some(Credential::OAuth(oauth)) = &stored {
            let Some(oauth_auth) = provider.auth().oauth.clone() else {
                return Ok(None);
            };
            if !allow_network || now_millis() < oauth.expires {
                return Ok(stored);
            }
            if signal.is_some_and(|t| t.is_cancelled()) {
                return Ok(None);
            }
            let now = now_millis();
            let signal = signal.cloned();
            let post = self
                .credentials
                .modify(
                    provider.id(),
                    Arc::new(move |current| {
                        let oauth_auth = oauth_auth.clone();
                        let signal = signal.clone();
                        Box::pin(async move {
                            match current {
                                // No change when the credential is missing or
                                // still valid (models.ts:343).
                                Some(Credential::OAuth(current)) if now < current.expires => {
                                    Ok(None)
                                }
                                Some(Credential::OAuth(current)) => oauth_auth
                                    .refresh(&current, signal.as_ref())
                                    .await
                                    .map(|credential| Some(Credential::OAuth(credential))),
                                _ => Ok(None),
                            }
                        })
                    }),
                )
                .await?;
            // A non-OAuth post-write value (no-op on a stale entry) means
            // "not refreshable" (models.ts:345).
            return Ok(post.filter(|credential| matches!(credential, Credential::OAuth(_))));
        }

        let Some(api_key) = provider.auth().api_key.clone() else {
            return Ok(None);
        };
        let credential = match &stored {
            Some(Credential::ApiKey(api_key_credential)) => Some(api_key_credential.clone()),
            _ => None,
        };
        let result = api_key
            .resolve(self.auth_context.as_ref(), credential.as_ref())
            .await?;
        let Some(result) = result else {
            return Ok(None);
        };
        Ok(Some(Credential::ApiKey(crate::auth::ApiKeyCredential {
            key: result.auth.api_key,
            env: result.env,
        })))
    }

    /// Offline cache restoration after a failed network refresh
    /// (models.ts:313-322): re-run the provider's refresh with
    /// `allowNetwork: false` so the stored overlay is published; errors are
    /// swallowed (best-effort).
    async fn restore_cached_overlay(
        &self,
        provider: &Arc<dyn Provider>,
        store: Arc<dyn ProviderModelsStore>,
        signal: Option<&CancellationToken>,
    ) {
        let context = RefreshModelsContext {
            credential: None,
            store,
            allow_network: false,
            force: false,
            signal: signal.cloned(),
        };
        if let Some(refresh) = provider.refresh_models(context) {
            let _ = refresh.await;
        }
    }

    fn require_provider(&self, model: &Model) -> Result<Arc<dyn Provider>, ModelsError> {
        self.get_provider(&model.provider).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Unknown provider: {}", model.provider),
            )
        })
    }

    /// `getAuth` — resolves provider auth plus static model headers.
    pub async fn get_auth(
        &self,
        model: &Model,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        let Some(provider) = self.get_provider(&model.provider) else {
            return Ok(None);
        };
        let result = resolve_provider_auth(
            provider.id(),
            provider.auth(),
            &self.credentials,
            &self.auth_context,
            overrides,
        )
        .await?;
        let Some(mut result) = result else {
            return Ok(None);
        };
        if let Some(model_headers) = &model.headers {
            let model_headers: ProviderHeaders = model_headers
                .iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect();
            result.auth.headers = merge_headers(result.auth.headers.as_ref(), Some(&model_headers));
        }
        Ok(Some(result))
    }

    /// `applyAuth`: resolves auth, merges headers (`transformHeaders` runs
    /// last), env and baseUrl into the request model/options.
    async fn apply_auth(
        &self,
        model: &Model,
        stream_options: Option<&StreamOptions>,
        transform_headers: Option<&TransformHeadersCallback>,
    ) -> Result<(Model, Option<StreamOptions>), ModelsError> {
        self.require_provider(model)?;
        let resolution = self
            .get_auth(
                model,
                Some(&AuthResolutionOverrides {
                    api_key: stream_options.and_then(|o| o.api_key.clone()),
                    env: stream_options.and_then(|o| o.env.clone()),
                }),
            )
            .await?;
        let Some(resolution) = resolution else {
            return Err(ModelsError::new(
                ModelsErrorCode::Auth,
                format!("Provider is not configured: {}", model.provider),
            ));
        };
        let auth = resolution.auth;

        // Explicit request options win per-field; the Models-only transform
        // runs last.
        let api_key = stream_options
            .and_then(|o| o.api_key.clone())
            .or(auth.api_key);
        let mut headers = merge_headers(
            auth.headers.as_ref(),
            stream_options.and_then(|o| o.headers.as_ref()),
        );
        if let Some(transform) = transform_headers {
            headers = Some(transform(headers.unwrap_or_default()).await);
        }
        let env = match (resolution.env, stream_options.and_then(|o| o.env.clone())) {
            (None, None) => None,
            (a, b) => {
                let mut merged = a.unwrap_or_default();
                merged.extend(b.unwrap_or_default());
                Some(merged)
            }
        };
        let request_model = match auth.base_url {
            Some(base_url) => Model {
                base_url,
                ..model.clone()
            },
            None => model.clone(),
        };
        // Upstream always builds an options object (`{ ...providerOptions,
        // apiKey, headers, env }`), even when the caller passed none.
        let request_options = {
            let mut options = stream_options.cloned().unwrap_or_default();
            options.api_key = api_key;
            options.headers = headers;
            options.env = env;
            Some(options)
        };

        Ok((request_model, request_options))
    }

    /// `stream` — resolves auth lazily behind the returned stream, then
    /// delegates to the owning provider.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> AssistantMessageEventStream {
        let this = self.clone();
        let model = model.clone();
        let context = context.clone();
        lazy_stream(&model.clone(), async move {
            let provider = this.require_provider(&model)?;
            let (request_model, request_options) = this
                .apply_auth(
                    &model,
                    options.as_ref().map(|o| &o.stream),
                    options.as_ref().and_then(|o| o.transform_headers.as_ref()),
                )
                .await?;
            Ok(provider.stream(&request_model, &context, request_options))
        })
    }

    /// `complete` — `stream(...).result()`.
    pub async fn complete(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> Option<crate::types::AssistantMessage> {
        self.stream(model, context, options).result().await
    }

    /// `streamSimple`.
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsSimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let this = self.clone();
        let model = model.clone();
        let context = context.clone();
        lazy_stream(&model.clone(), async move {
            let provider = this.require_provider(&model)?;
            let (request_model, request_options) = this
                .apply_auth(
                    &model,
                    options.as_ref().map(|o| &o.simple.stream),
                    options.as_ref().and_then(|o| o.transform_headers.as_ref()),
                )
                .await?;
            let simple_options = request_options.map(|stream| SimpleStreamOptions {
                stream,
                reasoning: options.as_ref().and_then(|o| o.simple.reasoning),
                thinking_budgets: options
                    .as_ref()
                    .and_then(|o| o.simple.thinking_budgets.clone()),
            });
            Ok(provider.stream_simple(&request_model, &context, simple_options))
        })
    }

    /// `completeSimple`.
    pub async fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsSimpleStreamOptions>,
    ) -> Option<crate::types::AssistantMessage> {
        self.stream_simple(model, context, options).result().await
    }
}

/// `createModels`.
pub fn create_models(options: Option<CreateModelsOptions>) -> Models {
    Models::new(options)
}

/// `ProviderModelsStore` scoped to one provider id (models.ts:287-291) —
/// providers cannot access other providers' catalogs.
pub struct ScopedModelsStore {
    store: Arc<dyn ModelsStore>,
    provider_id: String,
}

impl ScopedModelsStore {
    pub fn new(store: Arc<dyn ModelsStore>, provider_id: impl Into<String>) -> Self {
        Self {
            store,
            provider_id: provider_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl ProviderModelsStore for ScopedModelsStore {
    async fn read(&self) -> Result<Option<ModelsStoreEntry>, AiError> {
        self.store.read(&self.provider_id).await
    }

    async fn write(&self, entry: ModelsStoreEntry) -> Result<(), AiError> {
        self.store.write(&self.provider_id, entry).await
    }

    async fn delete(&self) -> Result<(), AiError> {
        self.store.delete(&self.provider_id).await
    }
}

const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

/// `getSupportedThinkingLevels`: `["off"]` for non-reasoning models; levels
/// mapped to `null` are unsupported; xhigh/max require an explicit mapping.
pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }

    EXTENDED_THINKING_LEVELS
        .into_iter()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(level));
            match mapped {
                // JSON null: level unsupported.
                Some(None) => false,
                // xhigh/max need an explicit mapped value.
                Some(Some(_)) => true,
                None => !matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max),
            }
        })
        .collect()
}

/// `clampThinkingLevel`: find the nearest available level, looking up first,
/// then down.
pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available_levels = get_supported_thinking_levels(model);
    if available_levels.contains(&level) {
        return level;
    }

    let Some(requested_index) = EXTENDED_THINKING_LEVELS.iter().position(|l| *l == level) else {
        return available_levels
            .first()
            .copied()
            .unwrap_or(ModelThinkingLevel::Off);
    };

    for candidate in &EXTENDED_THINKING_LEVELS[requested_index..] {
        if available_levels.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested_index].iter().rev() {
        if available_levels.contains(candidate) {
            return *candidate;
        }
    }
    available_levels
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

/// `modelsAreEqual`.
pub fn models_are_equal(a: Option<&Model>, b: Option<&Model>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.id == b.id && a.provider == b.provider,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::auth::{ApiKeyAuth, ModelAuth};
    use crate::types::{ApiKind, DoneReason, ModelThinkingLevel, StopReason, StreamEvent, Usage};

    fn model(provider: &str, api: &str) -> Model {
        serde_json::from_value(json!({
            "id": "m", "name": "m", "api": api, "provider": provider,
            "baseUrl": "https://example.com", "reasoning": false, "input": ["text"],
            "cost": {"input": 1.0, "output": 1.0, "cacheRead": 0.1, "cacheWrite": 1.0},
            "contextWindow": 1000, "maxTokens": 100
        }))
        .expect("model")
    }

    /// Model with a caller-chosen id (the generic [`model`] helper hardcodes
    /// `"m"`).
    fn model_with_id(provider: &str, id: &str) -> Model {
        serde_json::from_value(json!({
            "id": id, "name": id, "api": "anthropic-messages", "provider": provider,
            "baseUrl": "https://example.com", "reasoning": false, "input": ["text"],
            "cost": {"input": 1.0, "output": 1.0, "cacheRead": 0.1, "cacheWrite": 1.0},
            "contextWindow": 1000, "maxTokens": 100
        }))
        .expect("model")
    }

    struct StaticKeyAuth;

    #[async_trait::async_trait]
    impl ApiKeyAuth for StaticKeyAuth {
        fn name(&self) -> &str {
            "Test API key"
        }

        async fn resolve(
            &self,
            _ctx: &dyn AuthContext,
            _credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Result<Option<AuthResult>, ModelsError> {
            Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("sk-test".to_owned()),
                    headers: None,
                    base_url: None,
                },
                env: None,
                source: Some("TEST_API_KEY".to_owned()),
            }))
        }
    }

    struct EchoStreams;

    impl ProviderStreams for EchoStreams {
        fn stream(
            &self,
            model: &Model,
            _context: &Context,
            options: Option<StreamOptions>,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            let mut partial = crate::types::AssistantMessage {
                role: crate::types::AssistantRole::Assistant,
                content: vec![],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Usage::default(),
                stop_reason: StopReason::Pending,
                error_message: None,
                timestamp: 0,
            };
            stream.push(StreamEvent::Start {
                partial: partial.clone(),
            });
            // Surface the resolved api key in the final message so tests can
            // assert auth application happened.
            partial.stop_reason = StopReason::Stop;
            partial.error_message = options.and_then(|o| o.api_key);
            stream.push(StreamEvent::Done {
                reason: DoneReason::Stop,
                message: partial,
            });
            stream.end(None);
            stream
        }

        fn stream_simple(
            &self,
            model: &Model,
            context: &Context,
            options: Option<SimpleStreamOptions>,
        ) -> AssistantMessageEventStream {
            self.stream(model, context, options.map(|o| o.stream))
        }
    }

    fn test_provider(id: &str, api: ProviderApi) -> Arc<dyn Provider> {
        create_provider(CreateProviderOptions {
            id: id.to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(StaticKeyAuth)),
                oauth: None,
            },
            models: vec![model(id, ApiKind::ANTHROPIC_MESSAGES)],
            api,
        })
    }

    #[tokio::test]
    async fn test_models_stream_applies_auth() {
        let models = Models::new(None);
        models.set_provider(test_provider(
            "test",
            ProviderApi::Single(Arc::new(EchoStreams)),
        ));
        let model = models.get_model("test", "m").expect("model");
        let result = models
            .complete(&model, &Context::default(), None)
            .await
            .expect("result");
        assert_eq!(result.error_message, Some("sk-test".to_owned()));
    }

    #[tokio::test]
    async fn test_models_stream_unknown_provider_stream_error() {
        let models = Models::new(None);
        let model = model("ghost", ApiKind::ANTHROPIC_MESSAGES);
        let events: Vec<StreamEvent> =
            futures::StreamExt::collect(models.stream(&model, &Context::default(), None)).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { error, .. } => {
                assert_eq!(error.stop_reason, StopReason::Error);
                assert_eq!(
                    error.error_message,
                    Some("Unknown provider: ghost".to_owned())
                );
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_create_provider_missing_api_stream_error() {
        let models = Models::new(None);
        // Provider only implements openai-completions; the model asks for
        // anthropic-messages.
        let mut map = HashMap::new();
        map.insert(
            ApiKind::OPENAI_COMPLETIONS.to_owned(),
            Arc::new(EchoStreams) as Arc<dyn ProviderStreams>,
        );
        models.set_provider(test_provider("test", ProviderApi::Map(map)));
        let model = models.get_model("test", "m").expect("model");
        let events: Vec<StreamEvent> =
            futures::StreamExt::collect(models.stream(&model, &Context::default(), None)).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { error, .. } => assert_eq!(
                error.error_message,
                Some(
                    "Provider test has no API implementation for \"anthropic-messages\"".to_owned()
                )
            ),
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_create_provider_api_map_dispatch() {
        let models = Models::new(None);
        let mut map = HashMap::new();
        map.insert(
            ApiKind::ANTHROPIC_MESSAGES.to_owned(),
            Arc::new(EchoStreams) as Arc<dyn ProviderStreams>,
        );
        models.set_provider(test_provider("test", ProviderApi::Map(map)));
        let model = models.get_model("test", "m").expect("model");
        let result = models
            .complete(&model, &Context::default(), None)
            .await
            .expect("result");
        assert_eq!(result.stop_reason, StopReason::Stop);
    }

    fn thinking_model(map: Option<serde_json::Value>) -> Model {
        let mut value = json!({
            "id": "m", "name": "m", "api": "anthropic-messages", "provider": "p",
            "baseUrl": "https://example.com", "reasoning": true, "input": ["text"],
            "cost": {"input": 1.0, "output": 1.0, "cacheRead": 0.1, "cacheWrite": 1.0},
            "contextWindow": 1000, "maxTokens": 100
        });
        if let Some(map) = map {
            value["thinkingLevelMap"] = map;
        }
        serde_json::from_value(value).expect("model")
    }

    #[test]
    fn test_get_supported_thinking_levels() {
        let model = thinking_model(None);
        assert_eq!(
            get_supported_thinking_levels(&model),
            vec![
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Minimal,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
            ]
        );

        // null marks unsupported; explicit value enables xhigh.
        let model = thinking_model(Some(json!({"minimal": null, "xhigh": "xhigh"})));
        assert_eq!(
            get_supported_thinking_levels(&model),
            vec![
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Xhigh,
            ]
        );

        // Non-reasoning models only support off.
        let mut model = thinking_model(None);
        model.reasoning = false;
        assert_eq!(
            get_supported_thinking_levels(&model),
            vec![ModelThinkingLevel::Off]
        );
    }

    #[test]
    fn test_clamp_thinking_level_up_first_then_down() {
        // low unsupported: clamp up to medium (not down to minimal).
        let model = thinking_model(Some(json!({"low": null})));
        assert_eq!(
            clamp_thinking_level(&model, ModelThinkingLevel::Low),
            ModelThinkingLevel::Medium
        );
        // max unsupported and nothing above: clamp down to high.
        let model = thinking_model(None);
        assert_eq!(
            clamp_thinking_level(&model, ModelThinkingLevel::Max),
            ModelThinkingLevel::High
        );
        // Non-reasoning: everything clamps to off.
        let mut model = thinking_model(None);
        model.reasoning = false;
        assert_eq!(
            clamp_thinking_level(&model, ModelThinkingLevel::High),
            ModelThinkingLevel::Off
        );
    }

    // ------------------------------------------------------------------
    // refresh surface (models.ts:276-328; W6-C)
    // ------------------------------------------------------------------

    #[test]
    fn test_merge_models_overlays_dynamic_by_id() {
        let baseline = vec![model_with_id("p", "a"), model_with_id("p", "b")];
        let dynamic = vec![model_with_id("p", "b"), model_with_id("p", "c")];
        let merged = merge_models(&baseline, &dynamic);
        assert_eq!(
            merged.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        // Same-id dynamic entries replace the baseline entry in place.
        assert_eq!(merged[1].id, "b");
        assert_eq!(merged[1].provider, "p");
    }

    /// Fetch-based refreshable provider replicating the
    /// `createProvider.fetchModels` refresh body (models.ts:596-616; the
    /// constructor hook itself lands with T15, see module docs).
    struct FetchingTestProvider {
        id: String,
        baseline: Vec<Model>,
        auth: ProviderAuth,
        dynamic: Arc<Mutex<Vec<Model>>>,
        inflight: InflightRefresh,
        fetch_result: Arc<Mutex<Result<Vec<Model>, ModelsError>>>,
        fetch_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FetchingTestProvider {
        fn new(
            id: &str,
            baseline: Vec<Model>,
            fetch_result: Result<Vec<Model>, ModelsError>,
        ) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_owned(),
                baseline,
                auth: ProviderAuth {
                    api_key: Some(Arc::new(StaticKeyAuth)),
                    oauth: None,
                },
                dynamic: Arc::new(Mutex::new(Vec::new())),
                inflight: InflightRefresh::new(),
                fetch_result: Arc::new(Mutex::new(fetch_result)),
                fetch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })
        }
    }

    impl Provider for FetchingTestProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn name(&self) -> &str {
            &self.id
        }

        fn base_url(&self) -> Option<&str> {
            None
        }

        fn headers(&self) -> Option<&ProviderHeaders> {
            None
        }

        fn auth(&self) -> &ProviderAuth {
            &self.auth
        }

        fn get_models(&self) -> Vec<Model> {
            let dynamic = self.dynamic.lock().unwrap_or_else(|e| e.into_inner());
            merge_models(&self.baseline, &dynamic)
        }

        fn refresh_models(
            &self,
            context: RefreshModelsContext,
        ) -> Option<BoxFuture<'_, Result<(), ModelsError>>> {
            let id = self.id.clone();
            let dynamic = self.dynamic.clone();
            let fetch_result = self.fetch_result.clone();
            let fetch_calls = self.fetch_calls.clone();
            let inflight = &self.inflight;
            Some(Box::pin(async move {
                inflight
                    .join_or_run(async move {
                        let stored = context
                            .store
                            .read()
                            .await
                            .map_err(ai_error_to_models_error)?;
                        if let Some(stored) = &stored {
                            *dynamic.lock().unwrap_or_else(|e| e.into_inner()) = stored
                                .models
                                .iter()
                                .filter(|m| m.provider == id)
                                .cloned()
                                .collect();
                        }
                        if !context.allow_network
                            || context.signal.as_ref().is_some_and(|t| t.is_cancelled())
                        {
                            return Ok(());
                        }
                        fetch_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let refreshed = fetch_result
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()?;
                        if context.signal.as_ref().is_some_and(|t| t.is_cancelled()) {
                            return Ok(());
                        }
                        *dynamic.lock().unwrap_or_else(|e| e.into_inner()) = refreshed.clone();
                        context
                            .store
                            .write(ModelsStoreEntry {
                                models: refreshed,
                                last_modified: None,
                                checked_at: Some(now_millis()),
                                etag: None,
                            })
                            .await
                            .map_err(ai_error_to_models_error)
                    })
                    .await
            }))
        }

        fn stream(
            &self,
            _model: &Model,
            _context: &Context,
            _options: Option<StreamOptions>,
        ) -> AssistantMessageEventStream {
            unreachable!("not used in refresh tests")
        }

        fn stream_simple(
            &self,
            _model: &Model,
            _context: &Context,
            _options: Option<SimpleStreamOptions>,
        ) -> AssistantMessageEventStream {
            unreachable!("not used in refresh tests")
        }
    }

    #[tokio::test]
    async fn test_models_refresh_fetches_and_persists() {
        let models = Models::new(Some(CreateModelsOptions {
            credentials: None,
            auth_context: None,
            models_store: Some(Arc::new(InMemoryModelsStore::new())),
        }));
        let provider = FetchingTestProvider::new(
            "fetchy",
            vec![model_with_id("fetchy", "static")],
            Ok(vec![model_with_id("fetchy", "dynamic")]),
        );
        models.set_provider(provider.clone());

        let result = models.refresh(None).await;
        assert!(!result.aborted);
        assert!(result.errors.is_empty());
        let ids: Vec<String> = models
            .get_models(Some("fetchy"))
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["static".to_owned(), "dynamic".to_owned()]);
        assert_eq!(
            provider
                .fetch_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn test_models_refresh_offline_restores_stored_overlay() {
        // A stored overlay exists; the network fetch fails. The error is
        // recorded and the cached overlay is restored via the offline retry
        // (models.ts:313-322).
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        store
            .write(
                "fetchy",
                ModelsStoreEntry {
                    models: vec![model_with_id("fetchy", "cached")],
                    last_modified: None,
                    checked_at: Some(now_millis()),
                    etag: None,
                },
            )
            .await
            .expect("write");
        let models = Models::new(Some(CreateModelsOptions {
            credentials: None,
            auth_context: None,
            models_store: Some(store),
        }));
        let provider = FetchingTestProvider::new(
            "fetchy",
            vec![model_with_id("fetchy", "static")],
            Err(ModelsError::new(ModelsErrorCode::ModelSource, "boom")),
        );
        models.set_provider(provider.clone());

        let result = models.refresh(None).await;
        assert_eq!(
            result.errors,
            vec![("fetchy".to_owned(), "boom".to_owned())]
        );
        // The overlay survives the failed fetch (offline recovery).
        let ids: Vec<String> = models
            .get_models(Some("fetchy"))
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["static".to_owned(), "cached".to_owned()]);
    }

    #[tokio::test]
    async fn test_models_refresh_skips_unconfigured_and_static_providers() {
        // Unconfigured (no resolvable credential) → skipped without errors.
        struct UnconfiguredAuth;
        #[async_trait::async_trait]
        impl ApiKeyAuth for UnconfiguredAuth {
            fn name(&self) -> &str {
                "Unconfigured"
            }

            async fn resolve(
                &self,
                _ctx: &dyn AuthContext,
                _credential: Option<&crate::auth::ApiKeyCredential>,
            ) -> Result<Option<AuthResult>, ModelsError> {
                Ok(None)
            }
        }
        let models = Models::new(None);
        let unconfigured = Arc::new(UnconfiguredProvider {
            inner: create_provider(CreateProviderOptions {
                id: "ghost".to_owned(),
                name: None,
                base_url: None,
                headers: None,
                auth: ProviderAuth {
                    api_key: Some(Arc::new(UnconfiguredAuth)),
                    oauth: None,
                },
                models: vec![model("ghost", "m")],
                api: ProviderApi::Single(Arc::new(EchoStreams)),
            }),
            inflight: InflightRefresh::new(),
        });
        models.set_provider(unconfigured);
        // Static provider (no refresh_models) — no credential work at all.
        models.set_provider(test_provider(
            "static",
            ProviderApi::Single(Arc::new(EchoStreams)),
        ));

        let result = models.refresh(None).await;
        assert!(result.errors.is_empty());
        assert!(!result.aborted);
    }

    #[tokio::test]
    async fn test_inflight_refresh_dedups_concurrent_runs() {
        let slot = Arc::new(InflightRefresh::new());
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let runs_a = runs.clone();
        let runs_b = runs.clone();
        let slot_a = slot.clone();
        let slot_b = slot.clone();
        let join_a = tokio::spawn(async move {
            slot_a
                .join_or_run(async move {
                    runs_a.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = start_tx.send(());
                    let _ = done_rx.await;
                    Ok(())
                })
                .await
        });
        let join_b = tokio::spawn(async move {
            slot_b
                .join_or_run(async move {
                    runs_b.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                })
                .await
        });
        // Wait until the first refresh is in flight, then join from the
        // second caller.
        start_rx.await.expect("started");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(runs.load(std::sync::atomic::Ordering::Relaxed), 1);
        let _ = done_tx.send(());
        join_a.await.expect("join a").expect("ok");
        join_b.await.expect("join b").expect("ok");
        // A third call after completion starts a fresh run.
        let runs_third = runs.clone();
        slot.join_or_run(async move {
            runs_third.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })
        .await
        .expect("fresh run");
        assert_eq!(runs.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    /// Refreshable provider whose auth never resolves (probe/credential
    /// ordering test).
    struct UnconfiguredProvider {
        inner: Arc<dyn Provider>,
        inflight: InflightRefresh,
    }

    impl Provider for UnconfiguredProvider {
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

        fn refresh_models(
            &self,
            _context: RefreshModelsContext,
        ) -> Option<BoxFuture<'_, Result<(), ModelsError>>> {
            let inflight = &self.inflight;
            Some(Box::pin(async move {
                inflight.join_or_run(async { Ok(()) }).await
            }))
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
}
