//! Port of `packages/ai/src/providers/radius.ts` @ pi 0.86.1+ (19451accd)
//! — Radius gateway provider: ships the static public catalog for the
//! default gateway and overlays the effective gateway catalog after
//! authentication (4d38031fb "ship Radius model catalog").
//!
//! W5 scope notes:
//! - Custom gateways do not inherit the public `radius.pi.dev` catalog
//!   (upstream docs/providers.md#radius): their baseline is empty and
//!   models come from the OAuth credential's cached `gatewayConfig` /
//!   `{gateway}/v1/config` refresh (`radius_config`).
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
//! - [`RadiusProvider`] holds the `dynamicModels` cell and `inflightRefresh`
//!   dedup slot (D-032 item 5 closes here); [`Provider::refresh_models`]
//!   restores the provider-scoped store, imports legacy `gatewayConfig`
//!   catalogs, then fetches `{gateway}/v1/config` with the effective
//!   credential. Only the dynamic overlay is written — the static baseline
//!   stays put and [`Provider::get_models`] merges the two (dynamic wins by
//!   id, new ids append; radius.ts:40-49 @ 4d38031fb).
//! - [`Models::refresh`] (models.ts:276-328) resolves the credential before
//!   the provider runs, so the Bearer key is the resolved access token.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::api::pi_messages::PiMessages;
use crate::auth::oauth::radius::RadiusOAuth;
use crate::auth::{env_api_key_auth, Credential, ModelsError, ProviderAuth};
use crate::models::{
    create_provider, now_millis, CreateProviderOptions, InflightRefresh, ModelsPublication,
    Provider, ProviderApi, RefreshModelsContext,
};
use crate::models_store::ModelsStoreEntry;
use crate::types::{Model, ProviderHeaders, SimpleStreamOptions, StreamOptions, TranscriptContext};
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
    // `baselineModels` (radius.ts:26-29 @ 4d38031fb): the generated public
    // catalog ships with the default gateway only; custom gateways start
    // empty. The baseline models keep the catalog's `radius` provider id
    // upstream via `flattenModelCatalog("radius", …)`; our vendored data
    // already carries `provider: "radius"` and `baseUrl: …/v1`.
    let baseline = if gateway == normalize_radius_gateway_url(DEFAULT_RADIUS_GATEWAY) {
        crate::generated::get_builtin_models("radius").to_vec()
    } else {
        Vec::new()
    };
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
        // `getModels: () => merged` — the merge of baseline + dynamic
        // happens in [`RadiusProvider::get_models`] below; the inner core's
        // list stays the credential-less dynamic start.
        models: get_radius_models(&id, None),
        api: ProviderApi::Single(Arc::new(PiMessages)),
        ..Default::default()
    });
    Arc::new(RadiusProvider {
        inner,
        gateway,
        baseline,
        // The `dynamicModels` closure cell (radius.ts:25): starts from the
        // credential-less list, replaced by `refresh_models`.
        dynamic_models: Arc::new(Mutex::new(get_radius_models(&id, None))),
        id,
        inflight: InflightRefresh::new(),
    })
}

/// Decorator retaining the normalized gateway URL, the static public
/// baseline catalog, and the dynamic overlay (`baselineModels` /
/// `dynamicModels` / `inflightRefresh` closure state, radius.ts:25-30);
/// everything else delegates to the [`create_provider`] core.
pub struct RadiusProvider {
    inner: Arc<dyn Provider>,
    gateway: String,
    id: String,
    /// `baselineModels` — read-only after construction (the static public
    /// catalog; empty for custom gateways).
    baseline: Vec<Model>,
    /// `dynamicModels` — written by `refresh_models` (stored restore, legacy
    /// import, gateway fetch).
    dynamic_models: Arc<Mutex<Vec<Model>>>,
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
        // `getModels()` (radius.ts:40-49 @ 4d38031fb): merge the static
        // baseline with the dynamic overlay — a dynamic entry replaces the
        // baseline entry of the same id in place, otherwise appends.
        let dynamic = self
            .dynamic_models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut merged = self.baseline.clone();
        for model in dynamic {
            match merged.iter().position(|entry| entry.id == model.id) {
                Some(index) => merged[index] = model,
                None => merged.push(model),
            }
        }
        merged
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
        let models = self.dynamic_models.clone();
        let inflight = &self.inflight;
        Some(Box::pin(async move {
            inflight
                .join_or_run(async move {
                    // Phase 1: restore from the stored snapshot
                    // (models.ts:375-383 @ 4181f66). Only the dynamic
                    // overlay cell is written; the static baseline stays.
                    let stored = context.stored.clone();
                    let dynamic = if let Some(stored) = &stored {
                        stored
                            .models
                            .iter()
                            .filter(|model| model.provider == id)
                            .cloned()
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };

                    // Apply the restored overlay to the in-memory models
                    // through the publish gate (radius.ts:36-48): a stale
                    // generation or cancelled signal skips the update and
                    // aborts the rest of the refresh.
                    if stored.is_some() {
                        let models_for_update = models.clone();
                        let applied = context
                            .publish
                            .publish(ModelsPublication {
                                persist: None,
                                update: Some(Box::new(move || {
                                    *models_for_update.lock().unwrap_or_else(|e| e.into_inner()) =
                                        dynamic;
                                })),
                            })
                            .await?;
                        if !applied {
                            return Ok(());
                        }
                    }

                    // Import catalogs cached by the pre-ModelsStore Radius
                    // implementation (radius.ts:42-49).
                    if stored.is_none() {
                        if let Some(Credential::OAuth(oauth)) = &context.credential {
                            let legacy = get_radius_models(&id, Some(oauth));
                            if !legacy.is_empty() {
                                let legacy_clone = legacy.clone();
                                let models_for_update = models.clone();
                                let applied = context
                                    .publish
                                    .publish(ModelsPublication {
                                        persist: Some(Some(ModelsStoreEntry {
                                            models: legacy,
                                            last_modified: None,
                                            checked_at: Some(now_millis()),
                                            etag: None,
                                        })),
                                        update: Some(Box::new(move || {
                                            *models_for_update
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner()) = legacy_clone;
                                        })),
                                    })
                                    .await?;
                                // radius.ts:49-62: a stale generation or
                                // cancelled signal aborts the rest of the
                                // refresh here as well.
                                if !applied {
                                    return Ok(());
                                }
                            }
                        }
                    }

                    if !context.allow_network || context.signal.is_cancelled() {
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
                        Some(&context.signal),
                    )
                    .await?;
                    if context.signal.is_cancelled() {
                        return Ok(());
                    }
                    let refreshed = get_radius_models_from_config(&id, &config);
                    let refreshed_clone = refreshed.clone();
                    let models_clone = models.clone();
                    context
                        .publish
                        .publish(ModelsPublication {
                            persist: Some(Some(ModelsStoreEntry {
                                models: refreshed,
                                last_modified: None,
                                checked_at: Some(now_millis()),
                                etag: None,
                            })),
                            update: Some(Box::new(move || {
                                *models_clone.lock().unwrap_or_else(|e| e.into_inner()) =
                                    refreshed_clone;
                            })),
                        })
                        .await?;
                    Ok(())
                })
                .await
        }))
    }

    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.inner.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: Option<SimpleStreamOptions>,
    ) -> Result<AssistantMessageEventStream, String> {
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
        // Ships the static public catalog for the default gateway
        // (radius-provider.test.ts "ships a static public catalog…",
        // 4d38031fb).
        let models = provider.get_models();
        assert!(!models.is_empty());
        assert!(models.iter().any(|model| model.id == "balanced"));
        let balanced = models
            .iter()
            .find(|model| model.id == "balanced")
            .expect("balanced");
        assert_eq!(balanced.provider, "radius");
        assert_eq!(balanced.api.as_str(), "rpi-messages");
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
        // Custom gateways do not inherit the public `radius.pi.dev` catalog
        // (radius-provider.test.ts "does not apply the public Radius catalog
        // to custom gateways").
        assert!(provider.get_models().is_empty());
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

    /// Build a test `RefreshModelsContext` with the new publish/store API.
    async fn make_context(
        store: Arc<dyn crate::models_store::ModelsStore>,
        credential: Option<Credential>,
        allow_network: bool,
        force: bool,
    ) -> crate::models::RefreshModelsContext {
        use crate::models::{PublishHandle, PublishShared};
        let stored = store.read("radius", None).await.unwrap_or(None);
        let signal = tokio_util::sync::CancellationToken::new();
        let shared = std::sync::Arc::new(PublishShared {
            provider_id: "radius".to_owned(),
            generation: 1,
            signal: signal.clone(),
            store: store.clone(),
            chain: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            refresh_generations: std::sync::Arc::new(std::sync::RwLock::new(
                [("radius".to_owned(), 1u64)].into(),
            )),
        });
        crate::models::RefreshModelsContext {
            credential,
            stored,
            publish: PublishHandle { shared },
            allow_network,
            force: if allow_network { Some(force) } else { None },
            signal,
        }
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
                None,
            )
            .await
            .expect("write");
        let provider = radius_provider_with(RadiusProviderOptions {
            gateway: Some("http://127.0.0.1:1".to_owned()), // unreachable: must not be fetched
            ..Default::default()
        });
        let context =
            make_context(store, Some(oauth_credential("access-token")), false, false).await;
        provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .expect("refresh");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["stored".to_owned()]);
    }

    /// "overlays a cached effective catalog without network access"
    /// (radius-provider.test.ts, 4d38031fb): on the default gateway the
    /// stored dynamic catalog overlays the static baseline — same-id entries
    /// are replaced in place, new ids append; baseline-only ids survive.
    #[tokio::test]
    async fn overlays_cached_catalog_on_the_static_baseline_without_network() {
        let stored_models = vec![
            {
                let mut model = radius_model("balanced");
                model.name = "Fresh Balanced".to_owned();
                model.context_window = 424_242;
                model.base_url = "https://radius.example/v1".to_owned();
                model
            },
            radius_model("organization-only"),
        ];
        let store: Arc<dyn crate::models_store::ModelsStore> =
            Arc::new(crate::models_store::InMemoryModelsStore::new());
        store
            .write(
                "radius",
                crate::models_store::ModelsStoreEntry {
                    models: stored_models,
                    last_modified: None,
                    checked_at: Some(now_millis()),
                    etag: None,
                },
                None,
            )
            .await
            .expect("write");
        let provider = radius_provider();
        let context =
            make_context(store, Some(oauth_credential("access-token")), false, false).await;
        provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .expect("refresh");
        let models = provider.get_models();
        // Baseline (27) + organization-only; `balanced` is replaced, not
        // duplicated.
        assert_eq!(models.len(), 27 + 1);
        assert_eq!(
            models.iter().filter(|model| model.id == "balanced").count(),
            1
        );
        let balanced = models
            .iter()
            .find(|model| model.id == "balanced")
            .expect("balanced");
        assert_eq!(balanced.name, "Fresh Balanced");
        assert_eq!(balanced.base_url, "https://radius.example/v1");
        assert_eq!(balanced.context_window, 424_242);
        assert!(models.iter().any(|model| model.id == "organization-only"));
        // Baseline-only ids survive the overlay.
        assert!(models.iter().any(|model| model.id == "precise"));
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
        let context = make_context(
            store.clone(),
            Some(oauth_credential("access-token")),
            true,
            true,
        )
        .await;
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
        let stored = store
            .read("radius", None)
            .await
            .expect("read")
            .expect("entry");
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
        let context = make_context(store.clone(), Some(credential), false, false).await;
        provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .expect("refresh");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["radius-large".to_owned()]);
        // The legacy catalog is persisted for future refreshes.
        assert!(store.read("radius", None).await.expect("read").is_some());
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
                None,
            )
            .await
            .expect("write");
        let gateway = MockGateway::start(500, serde_json::json!({"error": "boom"})).await;
        let provider = radius_provider_with(RadiusProviderOptions {
            gateway: Some(gateway.url.clone()),
            ..Default::default()
        });
        let context = make_context(store, Some(oauth_credential("access-token")), true, true).await;
        assert!(provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .is_err());
        // The restored list is retained despite the failed fetch.
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["radius-large".to_owned()]);
    }

    /// Phase-1 restore goes through the publish gate (radius.ts:38-47): with
    /// a stale generation the update is skipped, the stored catalog does not
    /// overwrite the in-memory list, and the refresh returns early.
    #[tokio::test]
    async fn phase1_restore_skips_update_on_stale_generation() {
        use crate::models::{PublishHandle, PublishShared};
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
                None,
            )
            .await
            .expect("write");
        let provider = radius_provider_with(RadiusProviderOptions {
            gateway: Some("http://127.0.0.1:1".to_owned()), // unreachable: must not be fetched
            ..Default::default()
        });
        let stored = store.read("radius", None).await.expect("read");
        let signal = tokio_util::sync::CancellationToken::new();
        // Context captured generation 1; a newer refresh bumped it to 2.
        let shared = std::sync::Arc::new(PublishShared {
            provider_id: "radius".to_owned(),
            generation: 1,
            signal: signal.clone(),
            store: store.clone(),
            chain: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            refresh_generations: std::sync::Arc::new(std::sync::RwLock::new(
                [("radius".to_owned(), 2u64)].into(),
            )),
        });
        let context = crate::models::RefreshModelsContext {
            credential: Some(oauth_credential("access-token")),
            stored,
            publish: PublishHandle { shared },
            allow_network: false,
            force: None,
            signal,
        };
        provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .expect("refresh");
        // The stale phase-1 restore must not overwrite the in-memory list.
        assert!(provider.get_models().is_empty());
    }

    /// The legacy credential-catalog import goes through the publish gate
    /// too (radius.ts:49-62): with a stale generation the update is skipped
    /// and the refresh returns early without touching the in-memory list or
    /// the store.
    #[tokio::test]
    async fn legacy_import_skips_update_on_stale_generation() {
        use crate::models::{PublishHandle, PublishShared};
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
            gateway: Some("http://127.0.0.1:1".to_owned()), // unreachable: must not be fetched
            ..Default::default()
        });
        let signal = tokio_util::sync::CancellationToken::new();
        // Context captured generation 1; a newer refresh bumped it to 2.
        let shared = std::sync::Arc::new(PublishShared {
            provider_id: "radius".to_owned(),
            generation: 1,
            signal: signal.clone(),
            store: store.clone(),
            chain: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            refresh_generations: std::sync::Arc::new(std::sync::RwLock::new(
                [("radius".to_owned(), 2u64)].into(),
            )),
        });
        let context = crate::models::RefreshModelsContext {
            credential: Some(credential),
            stored: None,
            publish: PublishHandle { shared },
            allow_network: false,
            force: None,
            signal,
        };
        provider
            .refresh_models(context)
            .expect("refresh")
            .await
            .expect("refresh");
        // The stale legacy import must neither overwrite the in-memory
        // list nor persist the catalog.
        assert!(provider.get_models().is_empty());
        assert!(store.read("radius", None).await.expect("read").is_none());
    }
}
