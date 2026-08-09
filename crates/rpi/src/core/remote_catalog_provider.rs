//! Port of `packages/coding-agent/src/core/remote-catalog-provider.ts`
//! @ pi 0.82.1 (2efa728) — persisted pi.dev catalog overlay for static
//! built-in providers: ETag revalidation (If-None-Match), a 4-hour refresh
//! TTL, `generatedAt` freshness comparison against the built-in catalog, and
//! inflight dedup.
//!
//! Intentional differences (D-036):
//! - `getPiUserAgent` loses the `node/{version}` runtime component: the
//!   runtime is a native binary, so `rust` stands in (`pi-user-agent.ts`).
//! - `parseCatalog` drops entries that fail typed deserialization (upstream
//!   keeps them via an unchecked cast; unrepresentable fields are simply not
//!   carried — same precedent as D-032's radius sanitize).
//! - The 15s create/refresh timeout lives in [`crate::core::model_runtime`]
//!   and the `update --models` command (upstream applies it there too via
//!   `modelRefreshTimeoutMs` / the CLI AbortController); the provider honors
//!   the shared `signal` for every network step.
//! - The `catalogBaseUrl` default is `https://resetpi.com` (ADR-0009); settings/env
//!   configurability is T14 (ADR-0002 §8).

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rpi_ai::auth::{ModelsError, ModelsErrorCode};
use rpi_ai::models::{merge_models, now_millis, InflightRefresh, Provider, RefreshModelsContext};
use rpi_ai::models_store::ModelsStoreEntry;
use rpi_ai::types::{Context, Model, ProviderHeaders, SimpleStreamOptions, StreamOptions};
use rpi_ai::utils::event_stream::AssistantMessageEventStream;
use tokio_util::sync::CancellationToken;

use crate::config::VERSION;

/// `REMOTE_CATALOG_REFRESH_INTERVAL_MS` — 4 hours.
pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: i64 = 4 * 60 * 60 * 1000;

/// `DEFAULT_CATALOG_BASE_URL` (remote-catalog-provider.ts:5).
pub const DEFAULT_CATALOG_BASE_URL: &str = "https://resetpi.com";

/// Resolve the remote catalog base URL (ADR-0002 §8, T14-W6a):
/// `RPI_MODEL_CATALOG_URL` env > `modelCatalogUrl` setting >
/// [`DEFAULT_CATALOG_BASE_URL`]; the literal `off` disables the remote
/// catalog entirely (`None` — callers must not construct the overlay, so no
/// network request can occur). Rpi-specific: upstream has no override
/// (model-runtime.ts:69 `catalogBaseUrl` is an internal option only).
///
/// Wired at `ModelRuntime::create` (D-052): built-in providers are seeded
/// and wrapped per model-runtime.ts:144-150; the services layer passes the
/// `modelCatalogUrl` setting through `CreateModelRuntimeOptions`
/// `catalog_base_url`, env-only callers (SDK, `update --models`) resolve
/// here with `None`.
pub fn model_catalog_endpoint(settings_url: Option<&str>) -> Option<String> {
    crate::config::endpoint_from_env(
        crate::config::ENV_MODEL_CATALOG_URL,
        settings_url,
        DEFAULT_CATALOG_BASE_URL,
    )
}

/// Store errors surface as `ModelsError("model_source", …)` on the refresh
/// path (same mapping as `Models::refresh`).
fn store_error(error: rpi_ai::error::AiError) -> ModelsError {
    ModelsError::new(ModelsErrorCode::ModelSource, error.to_string())
}

/// `remoteModels` (remote-catalog-provider.ts:32-41): the stored overlay is
/// ignored when the local built-in catalog is newer or as-new (the
/// `generatedAt` comparison).
fn remote_models(entry: Option<&ModelsStoreEntry>, local_generated_at: Option<i64>) -> Vec<Model> {
    let Some(entry) = entry else {
        return Vec::new();
    };
    if let Some(local) = local_generated_at {
        if entry.last_modified.is_none_or(|last| last <= local) {
            return Vec::new();
        }
    }
    entry.models.clone()
}

/// `parseCatalog` (remote-catalog-provider.ts:18-30): the catalog body is an
/// array of models, `{ "models": [...] }`, or a keyed object; entries must
/// be objects carrying `id`, and the provider id is stamped onto every
/// model.
fn parse_catalog(provider_id: &str, value: &serde_json::Value) -> Result<Vec<Model>, String> {
    let entries: Vec<&serde_json::Value> = if let Some(array) = value.as_array() {
        array.iter().collect()
    } else if let Some(models) = value.get("models").and_then(serde_json::Value::as_array) {
        models.iter().collect()
    } else if value.is_object() {
        value
            .as_object()
            .map(|object| object.values().collect())
            .unwrap_or_default()
    } else {
        return Err(format!(
            "Invalid model catalog for provider \"{provider_id}\""
        ));
    };
    let mut models = Vec::new();
    for entry in entries {
        if !entry.is_object() || entry.get("id").is_none() {
            continue;
        }
        if let Ok(mut model) = serde_json::from_value::<Model>(entry.clone()) {
            model.provider = provider_id.to_owned();
            models.push(model);
        }
        // Entries failing typed deserialization are dropped; upstream keeps
        // them through an unchecked cast (see module docs / D-036).
    }
    Ok(models)
}

/// `encodeURIComponent` — every byte except unreserved characters.
pub(crate) fn encode_uri_component(value: &str) -> String {
    const UNRESERVED: &[char] = &['-', '_', '.', '!', '~', '*', '\'', '(', ')'];
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric() || UNRESERVED.contains(&ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `getPiUserAgent` (utils/pi-user-agent.ts) — `pi/{version} ({platform};
/// {runtime}; {arch})` with the rpi naming (ADR-0001) and a `rust` runtime
/// marker (D-036).
fn pi_user_agent() -> String {
    format!(
        "rpi/{VERSION} ({}; rust; {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// `Last-Modified` → Unix epoch milliseconds; `0` when absent/unparseable
/// (upstream `Date.parse` → `NaN` → `0`).
fn parse_last_modified(response: &reqwest::Response) -> i64 {
    let Some(value) = response.headers().get(reqwest::header::LAST_MODIFIED) else {
        return 0;
    };
    let Ok(value) = value.to_str() else {
        return 0;
    };
    match httpdate::parse_http_date(value) {
        Ok(time) => time
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// Add a persisted pi.dev catalog overlay to a static built-in provider
/// (`withRemoteCatalog`, remote-catalog-provider.ts:44-123). `None` base URL
/// uses [`DEFAULT_CATALOG_BASE_URL`]; `local_generated_at` is
/// `getBuiltinModelDataGeneratedAt()` — overlays older than the built-in
/// catalog are never published.
pub fn with_remote_catalog(
    provider: Arc<dyn Provider>,
    catalog_base_url: Option<String>,
    local_generated_at: Option<i64>,
) -> Arc<RemoteCatalogProvider> {
    Arc::new(RemoteCatalogProvider {
        inner: provider,
        catalog_base_url: catalog_base_url.unwrap_or_else(|| DEFAULT_CATALOG_BASE_URL.to_owned()),
        local_generated_at,
        dynamic_models: Arc::new(Mutex::new(Vec::new())),
        inflight: InflightRefresh::new(),
    })
}

/// The remote-catalog overlay decorator: `get_models` merges the persisted
/// overlay over the inner provider's static catalog; `refresh_models`
/// restores/revalidates it against `{catalogBaseUrl}/api/models/providers/
/// {id}`.
pub struct RemoteCatalogProvider {
    inner: Arc<dyn Provider>,
    catalog_base_url: String,
    local_generated_at: Option<i64>,
    dynamic_models: Arc<Mutex<Vec<Model>>>,
    inflight: InflightRefresh,
}

impl RemoteCatalogProvider {
    /// The decorated static provider.
    pub fn inner(&self) -> &Arc<dyn Provider> {
        &self.inner
    }
}

impl Provider for RemoteCatalogProvider {
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

    fn auth(&self) -> &rpi_ai::auth::ProviderAuth {
        self.inner.auth()
    }

    fn get_models(&self) -> Vec<Model> {
        let dynamic = self
            .dynamic_models
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        merge_models(&self.inner.get_models(), &dynamic)
    }

    /// `refreshModels` (remote-catalog-provider.ts:55-121): restore the
    /// stored overlay, honor the 4-hour TTL, revalidate with the stored ETag
    /// (If-None-Match) and publish fetched catalogs only when they are newer
    /// than the built-in catalog (`generatedAt`).
    fn refresh_models(
        &self,
        context: RefreshModelsContext,
    ) -> Option<BoxFuture<'_, Result<(), ModelsError>>> {
        let inner = self.inner.clone();
        let catalog_base_url = self.catalog_base_url.clone();
        let local_generated_at = self.local_generated_at;
        let dynamic_models = self.dynamic_models.clone();
        let inflight = &self.inflight;
        Some(Box::pin(async move {
            inflight
                .join_or_run(async move {
                    let stored = context.store.read().await.map_err(store_error)?;
                    {
                        let mut dynamic = dynamic_models.lock().unwrap_or_else(|e| e.into_inner());
                        *dynamic = remote_models(stored.as_ref(), local_generated_at)
                            .into_iter()
                            .filter(|model| model.provider == inner.id())
                            .collect();
                    }

                    if !context.allow_network
                        || context.signal.as_ref().is_some_and(|t| t.is_cancelled())
                    {
                        return Ok(());
                    }
                    // The 4-hour freshness window (remote-catalog-provider.ts:
                    // 61-68): skip the network round-trip entirely unless
                    // forced.
                    let within_ttl = stored.as_ref().is_some_and(|stored| {
                        let Some(checked_at) = stored.checked_at else {
                            return false;
                        };
                        stored.last_modified.is_some()
                            && now_millis() - checked_at < REMOTE_CATALOG_REFRESH_INTERVAL_MS
                    });
                    if !context.force && within_ttl {
                        return Ok(());
                    }

                    // Only revalidate when a cached body backs the validator,
                    // so a 304 can never leave the overlay empty
                    // (remote-catalog-provider.ts:70-72).
                    let validator = stored
                        .as_ref()
                        .filter(|stored| !stored.models.is_empty())
                        .and_then(|stored| stored.etag.clone());
                    let url = catalog_url(&catalog_base_url, inner.id())?;
                    let mut request = reqwest::Client::new()
                        .get(url)
                        .header(reqwest::header::ACCEPT, "application/json")
                        .header(reqwest::header::USER_AGENT, pi_user_agent());
                    if let Some(validator) = &validator {
                        request = request.header(reqwest::header::IF_NONE_MATCH, validator);
                    }
                    let response = send_with_signal(request, context.signal.as_ref()).await?;
                    if context.signal.as_ref().is_some_and(|t| t.is_cancelled()) {
                        return Ok(());
                    }
                    let checked_at = now_millis();

                    // Unchanged: dynamic_models already holds the stored
                    // overlay, so only the freshness window moves.
                    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                        if let Some(stored) = &stored {
                            context
                                .store
                                .write(ModelsStoreEntry {
                                    checked_at: Some(checked_at),
                                    ..stored.clone()
                                })
                                .await
                                .map_err(store_error)?;
                        }
                        return Ok(());
                    }
                    let status = response.status();
                    if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::NOT_IMPLEMENTED
                    {
                        // Unavailable overlay: drop the validator so the next
                        // refresh re-downloads instead of 304-ing forever
                        // (remote-catalog-provider.ts:90-98).
                        context
                            .store
                            .write(ModelsStoreEntry {
                                models: stored
                                    .as_ref()
                                    .map(|stored| stored.models.clone())
                                    .unwrap_or_default(),
                                last_modified: Some(0),
                                checked_at: Some(checked_at),
                                etag: None,
                            })
                            .await
                            .map_err(store_error)?;
                        return Ok(());
                    }
                    if !status.is_success() {
                        // Transient failure: the cached body and its
                        // validator stay valid, so keep the etag and let the
                        // next refresh revalidate instead of downloading the
                        // catalog (remote-catalog-provider.ts:99-104).
                        context
                            .store
                            .write(ModelsStoreEntry {
                                models: stored
                                    .as_ref()
                                    .map(|stored| stored.models.clone())
                                    .unwrap_or_default(),
                                last_modified: stored.as_ref().and_then(|s| s.last_modified),
                                checked_at: Some(checked_at),
                                etag: stored.as_ref().and_then(|s| s.etag.clone()),
                            })
                            .await
                            .map_err(store_error)?;
                        return Err(ModelsError::new(
                            ModelsErrorCode::ModelSource,
                            format!("Model catalog request failed for {}: {status}", inner.id()),
                        ));
                    }

                    // Capture the validators before consuming the body
                    // (headers are available once the response arrives).
                    let last_modified = parse_last_modified(&response);
                    let etag = response
                        .headers()
                        .get(reqwest::header::ETAG)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let body: serde_json::Value = response.json().await.map_err(|error| {
                        ModelsError::with_cause(
                            ModelsErrorCode::ModelSource,
                            format!("Invalid model catalog from {catalog_base_url}"),
                            &error.to_string(),
                        )
                    })?;
                    let refreshed = parse_catalog(inner.id(), &body).map_err(|message| {
                        ModelsError::new(ModelsErrorCode::ModelSource, message)
                    })?;
                    if context.signal.as_ref().is_some_and(|t| t.is_cancelled()) {
                        return Ok(());
                    }
                    let entry = ModelsStoreEntry {
                        models: refreshed,
                        last_modified: Some(last_modified),
                        checked_at: Some(checked_at),
                        etag,
                    };
                    *dynamic_models.lock().unwrap_or_else(|e| e.into_inner()) =
                        remote_models(Some(&entry), local_generated_at);
                    context.store.write(entry).await.map_err(store_error)
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

/// `new URL('/api/models/providers/{encodeURIComponent(id)}', base)`.
fn catalog_url(catalog_base_url: &str, provider_id: &str) -> Result<url::Url, ModelsError> {
    let encoded = encode_uri_component(provider_id);
    url::Url::parse(catalog_base_url)
        .map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::ModelSource,
                format!("Invalid catalog base URL {catalog_base_url}"),
                &error.to_string(),
            )
        })?
        .join(&format!("/api/models/providers/{encoded}"))
        .map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::ModelSource,
                format!("Invalid catalog base URL {catalog_base_url}"),
                &error.to_string(),
            )
        })
}

/// Send honoring the shared abort signal; a mid-flight cancellation rejects
/// like the upstream `fetch(url, { signal })` AbortError.
async fn send_with_signal(
    request: reqwest::RequestBuilder,
    signal: Option<&CancellationToken>,
) -> Result<reqwest::Response, ModelsError> {
    let send = request.send();
    match signal {
        Some(token) => tokio::select! {
            () = token.cancelled() => Err(ModelsError::new(
                ModelsErrorCode::ModelSource,
                "Model catalog request aborted",
            )),
            response = send => response.map_err(|error| {
                ModelsError::with_cause(
                    ModelsErrorCode::ModelSource,
                    "Model catalog request failed",
                    &error.to_string(),
                )
            }),
        },
        None => send.await.map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::ModelSource,
                "Model catalog request failed",
                &error.to_string(),
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    //! Port of `packages/coding-agent/test/remote-catalog-provider.test.ts`
    //! — the upstream `vi.spyOn(globalThis, "fetch", …)` becomes a scripted
    //! loopback HTTP server (no real network).

    use std::sync::Mutex;

    use rpi_ai::models::{RefreshModelsContext, ScopedModelsStore};
    use rpi_ai::models_store::{
        InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ProviderModelsStore,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn model(id: &str) -> Model {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "api": "openai-completions", "provider": "test-provider",
            "baseUrl": "https://example.test/v1", "reasoning": false, "input": ["text"],
            "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
            "contextWindow": 1000, "maxTokens": 100
        }))
        .expect("model")
    }

    fn test_provider(
        catalog_base_url: &str,
        local_generated_at: Option<i64>,
    ) -> Arc<RemoteCatalogProvider> {
        let inner: Arc<dyn Provider> =
            rpi_ai::models::create_provider(rpi_ai::models::CreateProviderOptions {
                id: "test-provider".to_owned(),
                name: None,
                base_url: None,
                headers: None,
                auth: rpi_ai::auth::ProviderAuth {
                    api_key: Some(Arc::new(rpi_ai::auth::helpers::env_api_key_auth(
                        "Test API key",
                        &["RPI_TEST_REMOTE_CATALOG_KEY"],
                    ))),
                    oauth: None,
                },
                models: vec![model("static")],
                api: rpi_ai::models::ProviderApi::Single(Arc::new(
                    rpi_ai::api::openai_completions::OpenAiCompletions,
                )),
            });
        with_remote_catalog(inner, Some(catalog_base_url.to_owned()), local_generated_at)
    }

    async fn scoped_store(store: Arc<dyn ModelsStore>) -> Arc<dyn ProviderModelsStore> {
        Arc::new(ScopedModelsStore::new(store, "test-provider"))
    }

    fn context(store: Arc<dyn ProviderModelsStore>) -> RefreshModelsContext {
        RefreshModelsContext {
            credential: None,
            store,
            allow_network: true,
            force: false,
            signal: None,
        }
    }

    fn context_offline(store: Arc<dyn ProviderModelsStore>) -> RefreshModelsContext {
        RefreshModelsContext {
            allow_network: false,
            ..context(store)
        }
    }

    // ----- scripted loopback catalog server -----

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        path: String,
        if_none_match: Option<String>,
        user_agent: Option<String>,
    }

    #[derive(Clone)]
    struct ScriptedResponse {
        status: u16,
        body: String,
        headers: Vec<(String, String)>,
        delay: std::time::Duration,
    }

    impl ScriptedResponse {
        fn json(status: u16, body: serde_json::Value) -> Self {
            Self {
                status,
                body: body.to_string(),
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                delay: std::time::Duration::ZERO,
            }
        }

        fn with_headers(mut self, headers: Vec<(&str, &str)>) -> Self {
            self.headers = headers
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            self
        }
    }

    struct MockCatalogServer {
        url: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl MockCatalogServer {
        async fn start(responses: Vec<ScriptedResponse>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let responses: Arc<Mutex<Vec<ScriptedResponse>>> = Arc::new(Mutex::new(responses));
            let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let handler_responses = responses;
            let handler_requests = requests.clone();
            tokio::spawn(async move {
                let shutdown = async move {
                    let _ = shutdown_rx.await;
                };
                tokio::pin!(shutdown);
                loop {
                    tokio::select! {
                        () = &mut shutdown => break,
                        accepted = listener.accept() => {
                            let (socket, _) = accepted.expect("accept");
                            let responses = handler_responses.clone();
                            let requests = handler_requests.clone();
                            tokio::spawn(async move {
                                handle_connection(socket, responses, requests).await;
                            });
                        }
                    }
                }
            });
            Self {
                url: format!("http://{addr}"),
                requests,
                shutdown: Some(shutdown_tx),
            }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    impl Drop for MockCatalogServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    async fn handle_connection(
        mut socket: tokio::net::TcpStream,
        responses: Arc<Mutex<Vec<ScriptedResponse>>>,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    ) {
        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        let mut saw_head_end = false;
        while !saw_head_end {
            let read = match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            head.extend_from_slice(&buf[..read]);
            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                saw_head_end = true;
            }
        }
        let head_text = String::from_utf8_lossy(&head);
        let mut lines = head_text.split("\r\n");
        let path = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_owned();
        let mut if_none_match = None;
        let mut user_agent = None;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_owned();
                match name.as_str() {
                    "if-none-match" => if_none_match = Some(value),
                    "user-agent" => user_agent = Some(value),
                    _ => {}
                }
            }
        }
        requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(RecordedRequest {
                path,
                if_none_match,
                user_agent,
            });

        let response = {
            let mut queue = responses.lock().unwrap_or_else(|e| e.into_inner());
            if queue.is_empty() {
                ScriptedResponse {
                    status: 500,
                    body: "unexpected request".to_owned(),
                    headers: Vec::new(),
                    delay: std::time::Duration::ZERO,
                }
            } else {
                queue.remove(0)
            }
        };
        if !response.delay.is_zero() {
            tokio::time::sleep(response.delay).await;
        }
        let reason = match response.status {
            200 => "OK",
            304 => "Not Modified",
            404 => "Not Found",
            429 => "Too Many Requests",
            501 => "Not Implemented",
            _ => "Internal Server Error",
        };
        let mut head_out = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            reason,
            response.body.len()
        );
        for (name, value) in &response.headers {
            head_out.push_str(&format!("{name}: {value}\r\n"));
        }
        head_out.push_str("\r\n");
        let _ = socket.write_all(head_out.as_bytes()).await;
        let _ = socket.write_all(response.body.as_bytes()).await;
        let _ = socket.flush().await;
    }

    // ----- tests -----

    #[tokio::test]
    async fn parses_keyed_catalogs_sends_ua_and_observes_ttl_and_force() {
        // Upstream: "parses keyed catalogs, sends version headers, observes
        // the refresh TTL, and supports forced refreshes" — 3 refreshes, 2
        // fetches (the second is inside the 4h TTL).
        let server = MockCatalogServer::start(vec![
            ScriptedResponse::json(200, serde_json::json!({ "dynamic": model("dynamic") })),
            ScriptedResponse::json(200, serde_json::json!({ "dynamic": model("dynamic") })),
        ])
        .await;
        let provider = test_provider(&server.url, None);
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let ctx = context(scoped_store(store.clone()).await);

        provider
            .refresh_models(ctx.clone())
            .expect("refresh")
            .await
            .expect("refresh");
        provider
            .refresh_models(ctx.clone())
            .expect("refresh")
            .await
            .expect("refresh (ttl)");
        provider
            .refresh_models(RefreshModelsContext { force: true, ..ctx })
            .expect("refresh")
            .await
            .expect("forced refresh");

        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static", "dynamic"]);
        let stored = store
            .read("test-provider")
            .await
            .expect("read")
            .expect("entry");
        let ids: Vec<String> = stored.models.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["dynamic"]);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].path, "/api/models/providers/test-provider");
        let ua = requests[0].user_agent.as_deref().expect("user-agent");
        assert!(
            ua.contains(&format!("rpi/{VERSION}")),
            "user-agent {ua} missing rpi/{VERSION}"
        );
    }

    #[tokio::test]
    async fn prefers_the_newer_of_generated_and_remote_catalogs() {
        // Upstream: "prefers the newer of the generated and remote catalogs"
        // — a remote catalog older than the built-in generatedAt never
        // reaches the overlay.
        let local_generated_at = 1_785_000_000_000i64; // fixed epoch millis
        let older = httpdate::fmt_http_date(
            std::time::UNIX_EPOCH
                + std::time::Duration::from_millis((local_generated_at - 60_000) as u64),
        );
        let newer = httpdate::fmt_http_date(
            std::time::UNIX_EPOCH
                + std::time::Duration::from_millis((local_generated_at + 60_000) as u64),
        );
        let server = MockCatalogServer::start(vec![
            ScriptedResponse::json(200, serde_json::json!({ "old": model("old") }))
                .with_headers(vec![("last-modified", &older)]),
            ScriptedResponse::json(200, serde_json::json!({ "newer": model("newer") }))
                .with_headers(vec![("last-modified", &newer)]),
        ])
        .await;
        let provider = test_provider(&server.url, Some(local_generated_at));
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let ctx = context(scoped_store(store.clone()).await);

        provider
            .refresh_models(ctx.clone())
            .expect("refresh")
            .await
            .expect("refresh");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static"]);

        provider
            .refresh_models(RefreshModelsContext { force: true, ..ctx })
            .expect("refresh")
            .await
            .expect("forced refresh");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static", "newer"]);
        let stored = store
            .read("test-provider")
            .await
            .expect("read")
            .expect("entry");
        assert_eq!(stored.last_modified, Some(local_generated_at + 60_000));
    }

    #[tokio::test]
    async fn revalidates_stored_catalog_with_etag_and_keeps_overlay_on_304() {
        // Upstream: "revalidates a stored catalog with its etag and keeps
        // the overlay on 304".
        let server = MockCatalogServer::start(vec![
            ScriptedResponse::json(200, serde_json::json!({ "dynamic": model("dynamic") }))
                .with_headers(vec![("etag", "\"catalog-1\"")]),
            ScriptedResponse {
                status: 304,
                body: String::new(),
                headers: vec![("etag".to_owned(), "\"catalog-1\"".to_owned())],
                delay: std::time::Duration::ZERO,
            },
        ])
        .await;
        let provider = test_provider(&server.url, None);
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let ctx = context(scoped_store(store.clone()).await);

        provider
            .refresh_models(ctx.clone())
            .expect("refresh")
            .await
            .expect("refresh");
        let requests = server.requests();
        assert_eq!(requests[0].if_none_match, None);
        let stored = store
            .read("test-provider")
            .await
            .expect("read")
            .expect("entry");
        assert_eq!(stored.etag.as_deref(), Some("\"catalog-1\""));

        let checked_at = stored.checked_at;
        provider
            .refresh_models(RefreshModelsContext {
                force: true,
                ..ctx.clone()
            })
            .expect("refresh")
            .await
            .expect("forced refresh");

        let requests = server.requests();
        assert_eq!(requests[1].if_none_match.as_deref(), Some("\"catalog-1\""));
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static", "dynamic"]);
        let stored = store
            .read("test-provider")
            .await
            .expect("read")
            .expect("entry");
        let ids: Vec<String> = stored.models.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["dynamic"]);
        assert_eq!(stored.etag.as_deref(), Some("\"catalog-1\""));
        assert!(stored.checked_at >= checked_at);
    }

    #[tokio::test]
    async fn drops_stale_etag_when_overlay_becomes_unavailable() {
        // Upstream: "drops a stale etag when the overlay becomes
        // unavailable" (404/501).
        let server = MockCatalogServer::start(vec![
            ScriptedResponse::json(200, serde_json::json!({ "dynamic": model("dynamic") }))
                .with_headers(vec![("etag", "\"catalog-1\"")]),
            ScriptedResponse {
                status: 501,
                body: "not implemented".to_owned(),
                headers: Vec::new(),
                delay: std::time::Duration::ZERO,
            },
        ])
        .await;
        let provider = test_provider(&server.url, None);
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let ctx = context(scoped_store(store.clone()).await);

        provider
            .refresh_models(ctx.clone())
            .expect("refresh")
            .await
            .expect("refresh");
        provider
            .refresh_models(RefreshModelsContext { force: true, ..ctx })
            .expect("refresh")
            .await
            .expect("501 resolves");

        let stored = store
            .read("test-provider")
            .await
            .expect("read")
            .expect("entry");
        // The validator is dropped so the next refresh re-downloads; the
        // cached body itself stays valid (remote-catalog-provider.ts:90-98).
        assert_eq!(stored.etag, None);
        assert_eq!(stored.last_modified, Some(0));
        assert_eq!(stored.models.len(), 1);
    }

    #[tokio::test]
    async fn keeps_etag_and_overlay_after_transient_failure() {
        // Upstream: "keeps the etag and overlay after a transient failure" —
        // 429 rejects, the cached body + etag survive, and the next refresh
        // revalidates with If-None-Match.
        let server = MockCatalogServer::start(vec![
            ScriptedResponse::json(200, serde_json::json!({ "dynamic": model("dynamic") }))
                .with_headers(vec![("etag", "\"catalog-1\"")]),
            ScriptedResponse::json(429, serde_json::json!({ "error": "rate limited" })),
            ScriptedResponse {
                status: 304,
                body: String::new(),
                headers: vec![("etag".to_owned(), "\"catalog-1\"".to_owned())],
                delay: std::time::Duration::ZERO,
            },
        ])
        .await;
        let provider = test_provider(&server.url, None);
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let ctx = context(scoped_store(store.clone()).await);

        provider
            .refresh_models(ctx.clone())
            .expect("refresh")
            .await
            .expect("refresh");
        let error = provider
            .refresh_models(RefreshModelsContext {
                force: true,
                ..ctx.clone()
            })
            .expect("refresh")
            .await
            .expect_err("429 rejects");
        assert!(error.message.contains("429"), "message: {}", error.message);

        let stored = store
            .read("test-provider")
            .await
            .expect("read")
            .expect("entry");
        assert_eq!(stored.etag.as_deref(), Some("\"catalog-1\""));
        let ids: Vec<String> = stored.models.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["dynamic"]);

        provider
            .refresh_models(RefreshModelsContext { force: true, ..ctx })
            .expect("refresh")
            .await
            .expect("304 revalidation");
        assert_eq!(
            server.requests()[2].if_none_match.as_deref(),
            Some("\"catalog-1\"")
        );
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static", "dynamic"]);
    }

    #[tokio::test]
    async fn treats_unimplemented_routes_as_unavailable_overlay() {
        // Upstream: "treats unimplemented pi.dev catalog routes as an
        // unavailable overlay" — 501 resolves (no error) with an empty
        // persisted overlay.
        let server = MockCatalogServer::start(vec![ScriptedResponse {
            status: 501,
            body: "not implemented".to_owned(),
            headers: Vec::new(),
            delay: std::time::Duration::ZERO,
        }])
        .await;
        let provider = test_provider(&server.url, None);
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());

        provider
            .refresh_models(context(scoped_store(store.clone()).await))
            .expect("refresh")
            .await
            .expect("501 resolves");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static"]);
        let stored = store
            .read("test-provider")
            .await
            .expect("read")
            .expect("entry");
        assert!(stored.models.is_empty());
        assert!(stored.checked_at.is_some());
    }

    #[tokio::test]
    async fn offline_refresh_restores_stored_overlay_without_network() {
        // `allowNetwork: false` restores the persisted overlay and never
        // fetches (models.ts:58-60 + remote-catalog-provider.ts:59-60).
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        store
            .write(
                "test-provider",
                ModelsStoreEntry {
                    models: vec![model("cached")],
                    last_modified: Some(now_millis()),
                    checked_at: Some(now_millis()),
                    etag: Some("\"catalog-1\"".to_owned()),
                },
            )
            .await
            .expect("write");
        let provider = test_provider("http://catalog.invalid", None);
        provider
            .refresh_models(context_offline(scoped_store(store).await))
            .expect("refresh")
            .await
            .expect("offline refresh");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static", "cached"]);
    }

    #[tokio::test]
    async fn inflight_refresh_dedups_concurrent_calls() {
        // Two concurrent refreshes share one fetch
        // (`inflightRefresh ??= …`, remote-catalog-provider.ts:56).
        let server = MockCatalogServer::start(vec![ScriptedResponse {
            status: 200,
            body: serde_json::json!({ "dynamic": model("dynamic") }).to_string(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            // Keep the fetch in flight so both callers overlap.
            delay: std::time::Duration::from_millis(150),
        }])
        .await;
        let provider = test_provider(&server.url, None);
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let ctx = context(scoped_store(store).await);
        let provider = Arc::new(provider);

        let provider_a = provider.clone();
        let ctx_a = ctx.clone();
        let a =
            tokio::spawn(async move { provider_a.refresh_models(ctx_a).expect("refresh").await });
        let provider_b = provider.clone();
        let b = tokio::spawn(async move { provider_b.refresh_models(ctx).expect("refresh").await });
        a.await.expect("a").expect("refresh a");
        b.await.expect("b").expect("refresh b");
        assert_eq!(server.requests().len(), 1);
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static", "dynamic"]);
    }

    #[tokio::test]
    async fn abort_during_fetch_rejects_and_skips_the_store_write() {
        // The shared signal aborts a slow catalog fetch; nothing is written
        // and the cached overlay stays put.
        let server = MockCatalogServer::start(vec![ScriptedResponse {
            status: 200,
            body: serde_json::json!({ "dynamic": model("dynamic") }).to_string(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            delay: std::time::Duration::from_secs(30),
        }])
        .await;
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        store
            .write(
                "test-provider",
                ModelsStoreEntry {
                    models: vec![model("cached")],
                    last_modified: Some(now_millis()),
                    // Outside the 4h freshness window so the fetch runs.
                    checked_at: Some(now_millis() - REMOTE_CATALOG_REFRESH_INTERVAL_MS * 2),
                    etag: None,
                },
            )
            .await
            .expect("write");
        let provider = test_provider(&server.url, None);
        let token = tokio_util::sync::CancellationToken::new();
        let token_for_abort = token.clone();
        let abort = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token_for_abort.cancel();
        });
        let result = provider
            .refresh_models(RefreshModelsContext {
                signal: Some(token),
                ..context(scoped_store(store.clone()).await)
            })
            .expect("refresh")
            .await;
        abort.await.expect("abort");
        assert!(result.is_err());
        let stored = store
            .read("test-provider")
            .await
            .expect("read")
            .expect("entry");
        let ids: Vec<String> = stored.models.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["cached"]);
    }

    #[tokio::test]
    async fn parses_array_and_models_object_catalogs() {
        // The other two accepted catalog shapes (remote-catalog-provider.ts:
        // 19-25): a bare array and `{ "models": [...] }`.
        let server = MockCatalogServer::start(vec![
            ScriptedResponse::json(200, serde_json::json!([model("array-model")])),
            ScriptedResponse::json(
                200,
                serde_json::json!({ "models": [model("object-model")] }),
            ),
        ])
        .await;
        let provider = test_provider(&server.url, None);
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let ctx = context(scoped_store(store.clone()).await);

        provider
            .refresh_models(ctx.clone())
            .expect("refresh")
            .await
            .expect("array catalog");
        provider
            .refresh_models(RefreshModelsContext { force: true, ..ctx })
            .expect("refresh")
            .await
            .expect("object catalog");
        let ids: Vec<String> = provider.get_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["static", "object-model"]);
    }

    #[tokio::test]
    async fn invalid_catalog_shape_rejects() {
        let server =
            MockCatalogServer::start(vec![ScriptedResponse::json(200, serde_json::json!(42))])
                .await;
        let provider = test_provider(&server.url, None);
        let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
        let error = provider
            .refresh_models(context(scoped_store(store).await))
            .expect("refresh")
            .await
            .expect_err("invalid catalog");
        assert!(
            error
                .message
                .contains("Invalid model catalog for provider \"test-provider\""),
            "message: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn encode_uri_component_matches_encode_uri_component() {
        assert_eq!(encode_uri_component("test-provider"), "test-provider");
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_uri_component("un~res'erved!*()"), "un~res'erved!*()");
        assert_eq!(encode_uri_component("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn remote_models_honors_generated_at() {
        let entry = ModelsStoreEntry {
            models: vec![model("m")],
            last_modified: Some(100),
            checked_at: None,
            etag: None,
        };
        // Newer remote catalog: overlay applies.
        assert_eq!(remote_models(Some(&entry), Some(50)).len(), 1);
        // Same-age remote catalog: overlay suppressed.
        assert!(remote_models(Some(&entry), Some(100)).is_empty());
        // Older remote catalog: overlay suppressed.
        assert!(remote_models(Some(&entry), Some(200)).is_empty());
        // Missing lastModified: overlay suppressed when generatedAt is known.
        let entry = ModelsStoreEntry {
            last_modified: None,
            ..entry
        };
        assert!(remote_models(Some(&entry), Some(200)).is_empty());
        // No local generatedAt: overlay always applies.
        assert_eq!(remote_models(Some(&entry), None).len(), 1);
        assert!(remote_models(None, None).is_empty());
    }

    #[test]
    fn refresh_interval_constant_is_four_hours() {
        assert_eq!(REMOTE_CATALOG_REFRESH_INTERVAL_MS, 4 * 60 * 60 * 1000);
    }

    // ---- T14-W6a: catalog endpoint resolution (ADR-0002 §8) ----

    /// Read-only env use: no test writes `RPI_MODEL_CATALOG_URL` (the
    /// env-override logic is covered by the pure
    /// [`crate::config::resolve_endpoint`] tests).
    #[test]
    fn model_catalog_endpoint_defaults_and_settings_override() {
        assert_eq!(
            crate::config::ENV_MODEL_CATALOG_URL,
            "RPI_MODEL_CATALOG_URL"
        );
        assert_eq!(
            model_catalog_endpoint(None).as_deref(),
            Some(DEFAULT_CATALOG_BASE_URL)
        );
        assert_eq!(
            model_catalog_endpoint(Some("https://mirror.test")).as_deref(),
            Some("https://mirror.test")
        );
        // Settings `off` disables the endpoint.
        assert_eq!(model_catalog_endpoint(Some("off")), None);
    }
}
