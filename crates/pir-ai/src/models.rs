//! Port of `packages/ai/src/models.ts` @ pi 0.82.1 (2efa728) — T03 scope:
//! `Provider` / `ProviderStreams` traits, `create_provider` with api-map
//! dispatch, `Models` (auth application + stream/stream_simple dispatch),
//! thinking-level helpers.
//!
//! Out of T03 scope (upstream features landing later): dynamic provider
//! overlay / `refreshModels` / `inflightRefresh` dedup (T13), `filterModels`
//! (T13), `checkAuth` / `getAvailable` / `login` / `logout` (T04).
//!
//! Intentional differences:
//! - `ApiStreamOptions<TApi>` per-API extras collapse to `StreamOptions`
//!   (Azure extras arrive with the Azure adapter, T13).
//! - The trait for API implementations is named `ProviderStreams` (upstream
//!   name); design §3.3 calls it `ApiStream` — same concept.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::auth::{
    resolve_provider_auth, AuthContext, AuthResolutionOverrides, AuthResult, CredentialStore,
    DefaultAuthContext, InMemoryCredentialStore, ModelsError, ModelsErrorCode, ProviderAuth,
};
use crate::models_json::OrderedMap;
use crate::types::{
    Context, Model, ModelThinkingLevel, ProviderHeaders, SimpleStreamOptions, StreamOptions,
};
use crate::utils::event_stream::AssistantMessageEventStream;
use crate::utils::headers::merge_headers;

use super::api::lazy::lazy_stream;

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
        });
        Self {
            providers: Arc::new(RwLock::new(OrderedMap::default())),
            credentials: options
                .credentials
                .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new())),
            auth_context: options
                .auth_context
                .unwrap_or_else(|| Arc::new(DefaultAuthContext)),
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
}
