//! Port of `packages/ai/src/providers/radius.ts` @ pi 0.82.1 (2efa728) —
//! Radius gateway provider with a persisted, dynamically refreshed catalog.
//!
//! W5 scope notes:
//! - The static baseline catalog is empty: models come from the OAuth
//!   credential's cached `gatewayConfig` / `{gateway}/v1/config` refresh
//!   (`radius_config`).
//! - OAuth (`loadRadiusOAuth`: browser PKCE / device code against the
//!   normalized gateway) landed in T13 W5 as
//!   [`crate::auth::oauth::radius`], constructed here from the same
//!   normalized gateway the W6 refresh targets.
//! - Unlike the other factories, upstream builds the provider object
//!   literally (to hold refresh state); here [`create_provider`] builds the
//!   streaming core and [`RadiusProvider`] decorates it, retaining the
//!   normalized gateway URL.
//!
//! W6-C notes (`refreshModels` overlay, upstream radius.ts:36-63):
//! - [`RadiusProvider`] holds the `models` cell and `inflightRefresh` dedup
//!   slot (D-032 item 5 closes here); [`Provider::refresh_models`] restores
//!   the provider-scoped store, imports legacy `gatewayConfig` catalogs,
//!   then fetches `{gateway}/v1/config` with the effective credential.
//! - [`Models::refresh`] (models.ts:276-328) resolves the credential before
//!   the provider runs, so the Bearer key is the resolved access token.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::api::pi_messages::PiMessages;
use crate::auth::oauth::radius::RadiusOAuth;
use crate::auth::{env_api_key_auth, Credential, ModelsError, ProviderAuth};
use crate::models::{
    ai_error_to_models_error, create_provider, now_millis, CreateProviderOptions, InflightRefresh,
    Provider, ProviderApi, RefreshModelsContext,
};
use crate::models_store::ModelsStoreEntry;
use crate::types::{Context, Model, ProviderHeaders, SimpleStreamOptions, StreamOptions};
use crate::utils::event_stream::AssistantMessageEventStream;

use super::radius_config::{
    get_radius_models, get_radius_models_from_config, load_radius_gateway_config,
    normalize_radius_gateway_url, DEFAULT_RADIUS_GATEWAY,
};

/// `RadiusProviderOptions`.
#[derive(Debug, Clone, Default)]
pub struct RadiusProviderOptions {
    pub id: Option<String>,
    pub name: Option<String>,
    pub gateway: Option<String>,
}

/// `radiusProvider()` with default options: the built-in `"radius"` id.
pub fn radius_provider() -> Arc<dyn Provider> {
    radius_provider_with(RadiusProviderOptions::default())
}

/// `radiusProvider(options)` — returns the decorator concretely so callers
/// (and the W6 refresh wiring) can reach [`RadiusProvider::gateway`].
pub fn radius_provider_with(options: RadiusProviderOptions) -> Arc<RadiusProvider> {
    let id = options.id.unwrap_or_else(|| "radius".to_owned());
    let name = options.name.unwrap_or_else(|| "Radius".to_owned());
    let gateway =
        normalize_radius_gateway_url(options.gateway.as_deref().unwrap_or(DEFAULT_RADIUS_GATEWAY));
    let inner = create_provider(CreateProviderOptions {
        id: id.clone(),
        name: Some(name.clone()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(env_api_key_auth(
                "Radius API key",
                &["RADIUS_API_KEY"],
            ))),
            // `lazyOAuth({ name, load: () => loadRadiusOAuth({ name, gateway }) })`
            // — the gateway is already normalized, `RadiusOAuth::new`
            // re-normalizes idempotently.
            oauth: Some(Arc::new(RadiusOAuth::new(&name, &gateway))),
        },
        // `getModels: () => models` starts as `getRadiusModels(id, undefined)`
        // — empty without an OAuth credential's cached gateway config.
        models: get_radius_models(&id, None),
        api: ProviderApi::Single(Arc::new(PiMessages)),
    });
    Arc::new(RadiusProvider {
        inner,
        gateway,
        // The `models` closure cell (radius.ts:24): starts from the
        // credential-less list, replaced by `refresh_models`.
        models: Arc::new(Mutex::new(get_radius_models(&id, None))),
        id,
        inflight: InflightRefresh::new(),
    })
}

/// Decorator retaining the normalized gateway URL and the dynamic catalog
/// overlay (`models` / `inflightRefresh` closure state, radius.ts:24-25);
/// everything else delegates to the [`create_provider`] core.
pub struct RadiusProvider {
    inner: Arc<dyn Provider>,
    gateway: String,
    id: String,
    models: Arc<Mutex<Vec<Model>>>,
    inflight: InflightRefresh,
}

impl RadiusProvider {
    /// Normalized gateway URL (`normalizeRadiusGatewayUrl`) — the
    /// `refreshModels` overlay fetches `{gateway}/v1/config` and the OAuth
    /// flow targets this gateway.
    pub fn gateway(&self) -> &str {
        &self.gateway
    }
}

impl Provider for RadiusProvider {
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
        self.models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn filter_models(&self, models: Vec<Model>, credential: Option<&Credential>) -> Vec<Model> {
        self.inner.filter_models(models, credential)
    }

    /// `refreshModels` (radius.ts:36-63): restore the provider-scoped stored
    /// catalog, import the pre-ModelsStore `gatewayConfig` catalog from an
    /// OAuth credential, then fetch `{gateway}/v1/config` with the effective
    /// credential when network access is allowed.
    fn refresh_models(
        &self,
        context: RefreshModelsContext,
    ) -> Option<BoxFuture<'_, Result<(), ModelsError>>> {
        let id = self.id.clone();
        let gateway = self.gateway.clone();
        let models = self.models.clone();
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
                        *models.lock().unwrap_or_else(|e| e.into_inner()) = stored
                            .models
                            .iter()
                            .filter(|model| model.provider == id)
                            .cloned()
                            .collect();
                    }

                    // Import catalogs cached by the pre-ModelsStore Radius
                    // implementation (radius.ts:42-49).
                    if stored.is_none() {
                        if let Some(Credential::OAuth(oauth)) = &context.credential {
                            let legacy = get_radius_models(&id, Some(oauth));
                            if !legacy.is_empty() {
                                *models.lock().unwrap_or_else(|e| e.into_inner()) = legacy.clone();
                                context
                                    .store
                                    .write(ModelsStoreEntry {
                                        models: legacy,
                                        last_modified: None,
                                        checked_at: Some(now_millis()),
                                        etag: None,
                                    })
                                    .await
                                    .map_err(ai_error_to_models_error)?;
                            }
                        }
                    }

                    if !context.allow_network
                        || context.signal.as_ref().is_some_and(|t| t.is_cancelled())
                    {
                        return Ok(());
                    }
                    let api_key = match &context.credential {
                        Some(Credential::OAuth(oauth)) => Some(oauth.access.clone()),
                        Some(Credential::ApiKey(api_key)) => api_key.key.clone(),
                        None => None,
                    };
                    let config = load_radius_gateway_config(
                        &gateway,
                        api_key.as_deref(),
                        context.signal.as_ref(),
                    )
                    .await?;
                    if context.signal.as_ref().is_some_and(|t| t.is_cancelled()) {
                        return Ok(());
                    }
                    let refreshed = get_radius_models_from_config(&id, &config);
                    *models.lock().unwrap_or_else(|e| e.into_inner()) = refreshed.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_match_upstream() {
        let provider = radius_provider();
        assert_eq!(provider.id(), "radius");
        assert_eq!(provider.name(), "Radius");
        // Purely dynamic: empty until refreshed (providers.test.ts:41).
        assert!(provider.get_models().is_empty());
    }

    #[test]
    fn custom_options_normalize_the_gateway() {
        let provider = radius_provider_with(RadiusProviderOptions {
            id: Some("radius-eu".to_owned()),
            name: Some("Radius EU".to_owned()),
            gateway: Some("radius.eu.example.com/".to_owned()),
        });
        assert_eq!(provider.id(), "radius-eu");
        assert_eq!(provider.name(), "Radius EU");
        assert_eq!(provider.gateway(), "https://radius.eu.example.com");
    }

    // ------------------------------------------------------------------
    // refreshModels overlay (radius.ts:36-63; W6-C) — mock gateway over
    // loopback (upstream: `vi.stubGlobal("fetch", …)`).
    // ------------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        path: String,
        authorization: Option<String>,
    }

    struct MockGateway {
        url: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl MockGateway {
        async fn start(status: u16, body: serde_json::Value) -> Self {
            let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let handler_requests = requests.clone();
            let app = axum::Router::new().fallback(
                move |request: axum::http::Request<axum::body::Body>| {
                    let requests = handler_requests.clone();
                    let body = body.clone();
                    async move {
                        requests
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(RecordedRequest {
                                path: request.uri().path().to_owned(),
                                authorization: request
                                    .headers()
                                    .get(axum::http::header::AUTHORIZATION)
                                    .and_then(|value| value.to_str().ok())
                                    .map(str::to_owned),
                            });
                        (
                            axum::http::StatusCode::from_u16(status).expect("status"),
                            body.to_string(),
                        )
                    }
                },
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = rx.await;
                    })
                    .await;
            });
            Self {
                url: format!("http://{addr}"),
                requests,
                shutdown: Some(tx),
            }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    impl Drop for MockGateway {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    fn gateway_config_json(base_url: &str) -> serde_json::Value {
        serde_json::json!({
            "baseUrl": base_url,
            "models": [{
                "id": "radius-large",
                "name": "Radius Large",
                "reasoning": true,
                "input": ["text"],
                "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 0.2},
                "contextWindow": 200000,
                "maxTokens": 8192
            }]
        })
    }

    fn radius_model(id: &str) -> Model {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "api": "pi-messages", "provider": "radius",
            "baseUrl": "https://radius.pi.dev/api", "reasoning": false, "input": ["text"],
            "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 0.2},
            "contextWindow": 128000, "maxTokens": 16384
        }))
        .expect("model")
    }

    fn oauth_credential(access: &str) -> Credential {
        Credential::OAuth(crate::auth::OAuthCredential {
            refresh: "r".to_owned(),
            access: access.to_owned(),
            expires: i64::MAX,
            extra: serde_json::Map::new(),
        })
    }

    async fn scoped_store(
        store: Arc<dyn crate::models_store::ModelsStore>,
    ) -> Arc<dyn crate::models_store::ProviderModelsStore> {
        Arc::new(crate::models::ScopedModelsStore::new(store, "radius"))
    }

    #[tokio::test]
    async fn refresh_restores_stored_overlay_without_network() {
        let store: Arc<dyn crate::models_store::ModelsStore> =
            Arc::new(crate::models_store::InMemoryModelsStore::new());
        store
            .write(
                "radius",
                crate::models_store::ModelsStoreEntry {
                    models: vec![radius_model("stored")],
                    last_modified: None,
                    checked_at: Some(now_millis()),
                    etag: None,
                },
            )
            .await
            .expect("write");
        let provider = radius_provider_with(RadiusProviderOptions {
            gateway: Some("http://127.0.0.1:1".to_owned()), // unreachable: must not be fetched
            ..Default::default()
        });
        let context = crate::models::RefreshModelsContext {
            credential: Some(oauth_credential("access-token")),
            store: scoped_store(store).await,
            allow_network: false,
            force: false,
            signal: None,
        };
        provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .expect("refresh");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["stored".to_owned()]);
    }

    #[tokio::test]
    async fn refresh_fetches_gateway_config_with_bearer_and_persists() {
        let gateway =
            MockGateway::start(200, gateway_config_json("https://radius.pi.dev/api")).await;
        let store: Arc<dyn crate::models_store::ModelsStore> =
            Arc::new(crate::models_store::InMemoryModelsStore::new());
        let provider = radius_provider_with(RadiusProviderOptions {
            gateway: Some(gateway.url.clone()),
            ..Default::default()
        });
        let context = crate::models::RefreshModelsContext {
            credential: Some(oauth_credential("access-token")),
            store: scoped_store(store.clone()).await,
            allow_network: true,
            force: true,
            signal: None,
        };
        provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .expect("refresh");
        assert_eq!(gateway.requests().len(), 1);
        assert_eq!(gateway.requests()[0].path, "/v1/config");
        assert_eq!(
            gateway.requests()[0].authorization.as_deref(),
            Some("Bearer access-token")
        );
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["radius-large".to_owned()]);
        let stored = store.read("radius").await.expect("read").expect("entry");
        assert_eq!(stored.models.len(), 1);
        assert!(stored.checked_at.is_some());
        assert_eq!(stored.models[0].base_url, "https://radius.pi.dev/api");
    }

    #[tokio::test]
    async fn refresh_imports_legacy_credential_catalog_without_network() {
        // Pre-ModelsStore Radius catalogs live on the OAuth credential's
        // `gatewayConfig` extra (radius.ts:42-49).
        let mut extra = serde_json::Map::new();
        extra.insert(
            "gatewayConfig".to_owned(),
            gateway_config_json("https://radius.pi.dev/api"),
        );
        let credential = Credential::OAuth(crate::auth::OAuthCredential {
            refresh: "r".to_owned(),
            access: "a".to_owned(),
            expires: i64::MAX,
            extra,
        });
        let store: Arc<dyn crate::models_store::ModelsStore> =
            Arc::new(crate::models_store::InMemoryModelsStore::new());
        let provider = radius_provider_with(RadiusProviderOptions {
            gateway: Some("http://127.0.0.1:1".to_owned()),
            ..Default::default()
        });
        let context = crate::models::RefreshModelsContext {
            credential: Some(credential),
            store: scoped_store(store.clone()).await,
            allow_network: false,
            force: false,
            signal: None,
        };
        provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .expect("refresh");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["radius-large".to_owned()]);
        // The legacy catalog is persisted for future refreshes.
        assert!(store.read("radius").await.expect("read").is_some());
    }

    #[tokio::test]
    async fn refresh_keeps_previous_list_on_gateway_error() {
        let store: Arc<dyn crate::models_store::ModelsStore> =
            Arc::new(crate::models_store::InMemoryModelsStore::new());
        // Seed the current catalog via the store (a previous successful
        // refresh's output).
        store
            .write(
                "radius",
                crate::models_store::ModelsStoreEntry {
                    models: vec![radius_model("radius-large")],
                    last_modified: None,
                    checked_at: Some(now_millis()),
                    etag: None,
                },
            )
            .await
            .expect("write");
        let gateway = MockGateway::start(500, serde_json::json!({"error": "boom"})).await;
        let provider = radius_provider_with(RadiusProviderOptions {
            gateway: Some(gateway.url.clone()),
            ..Default::default()
        });
        let context = crate::models::RefreshModelsContext {
            credential: Some(oauth_credential("access-token")),
            store: scoped_store(store).await,
            allow_network: true,
            force: true,
            signal: None,
        };
        assert!(provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .is_err());
        // The restored list is retained despite the failed fetch.
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["radius-large".to_owned()]);
    }
}
