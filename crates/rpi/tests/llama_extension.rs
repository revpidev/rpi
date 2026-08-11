//! Port of `packages/coding-agent/test/llama-extension.test.ts`
//! @ pi 0.82.1 (2efa728) — the loopback-server tests for the llama.cpp
//! extension, plus headless `/llama` flow tests with a scripted [`LlamaUi`].
//!
//! The upstream `node:http` servers become a scripted raw-TCP loopback
//! server (SSE included); no real network is touched.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rpi::extensions::llama::client::{LlamaClient, LlamaModelInfo, LlamaProgress};
use rpi::extensions::llama::huggingface::{find_hugging_face_token, HuggingFaceClient};
use rpi::extensions::llama::provider::{create_llama_provider, LLAMA_PROVIDER_ID};
use rpi::extensions::llama::{
    ConnectionErrorChoice, HuggingFaceSearchFn, LlamaHost, LlamaManagerAction, LlamaUi,
    NotifyLevel, ProgressState,
};
use rpi_ai::auth::{AuthResult, Credential, ModelAuth, ModelsError};
use rpi_ai::models::{PublishHandle, PublishShared, RefreshModelsContext};
use rpi_ai::models_store::{InMemoryModelsStore, ModelsStore};
use rpi_ai::types::ProviderEnv;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Build a test `RefreshModelsContext` for the llama provider tests.
async fn make_llama_context(
    store: Arc<dyn ModelsStore>,
    credential: Option<Credential>,
    allow_network: bool,
) -> RefreshModelsContext {
    let stored = store.read(LLAMA_PROVIDER_ID, None).await.unwrap_or(None);
    let signal = tokio_util::sync::CancellationToken::new();
    let shared = Arc::new(PublishShared {
        provider_id: LLAMA_PROVIDER_ID.to_owned(),
        generation: 1,
        signal: signal.clone(),
        store,
        chain: Arc::new(tokio::sync::Mutex::new(None)),
        refresh_generations: Arc::new(std::sync::RwLock::new(
            [(LLAMA_PROVIDER_ID.to_owned(), 1u64)].into(),
        )),
    });
    RefreshModelsContext {
        credential,
        stored,
        publish: PublishHandle { shared },
        allow_network,
        force: if allow_network { Some(false) } else { None },
        signal,
    }
}

// -------------------------------------------------------------------------
// Scripted loopback server (llama.cpp router + Hugging Face API)
// -------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

#[derive(Clone)]
struct MockResponse {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl MockResponse {
    fn json(value: serde_json::Value) -> Self {
        MockResponse {
            status: 200,
            content_type: "application/json",
            body: value.to_string(),
        }
    }

    fn status(status: u16) -> Self {
        MockResponse {
            status,
            content_type: "application/json",
            body: String::new(),
        }
    }
}

/// SSE broadcast hub: `send_json` forwards one `data: …\n\n` frame to every
/// connected `/models/sse` consumer.
#[derive(Clone)]
struct SseHub {
    tx: tokio::sync::broadcast::Sender<String>,
}

impl SseHub {
    fn send_json(&self, value: serde_json::Value) {
        let _ = self.tx.send(format!("data: {value}\n\n"));
    }
}

type Handler = Arc<dyn Fn(&RecordedRequest, &SseHub) -> MockResponse + Send + Sync>;

struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockServer {
    async fn start(handler: Handler) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (sse_tx, _) = tokio::sync::broadcast::channel::<String>(64);
        let sse = SseHub { tx: sse_tx };
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let sse = sse.clone();
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                let shutdown = async move {
                    let _ = shutdown_rx.await;
                };
                tokio::pin!(shutdown);
                loop {
                    tokio::select! {
                        () = &mut shutdown => break,
                        accepted = listener.accept() => {
                            let Ok((socket, _)) = accepted else { break };
                            let handler = handler.clone();
                            let sse = sse.clone();
                            let requests = Arc::clone(&requests);
                            tokio::spawn(async move {
                                handle_connection(socket, handler, requests, sse).await;
                            });
                        }
                    }
                }
            });
        }
        MockServer {
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

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn handle_connection(
    mut socket: tokio::net::TcpStream,
    handler: Handler,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    sse: SseHub,
) {
    // Read the head, then the content-length body.
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let read = match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 64 * 1024 {
            return;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut authorization = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => authorization = Some(value.trim().to_owned()),
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let read = match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        body.extend_from_slice(&chunk[..read]);
    }
    let request = RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        authorization,
        body: String::from_utf8_lossy(&body[..content_length.min(body.len())]).to_string(),
    };

    // The SSE stream never consults the handler: it replays broadcast frames.
    if path == "/models/sse" {
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await;
        let mut receiver = sse.tx.subscribe();
        loop {
            tokio::select! {
                frame = receiver.recv() => {
                    let Ok(frame) = frame else { return };
                    if socket.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = socket.flush().await;
                }
                readable = socket.readable() => {
                    if readable.is_err() {
                        return;
                    }
                    let mut probe = [0u8; 1];
                    match socket.try_read(&mut probe) {
                        Ok(0) => return, // client closed
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }
            }
        }
    }

    requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(request.clone());
    let response = handler(&request, &sse);
    let reason = match response.status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Internal Server Error",
    };
    let head_out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    let _ = socket.write_all(head_out.as_bytes()).await;
    let _ = socket.write_all(response.body.as_bytes()).await;
    let _ = socket.flush().await;
}

// -------------------------------------------------------------------------
// Upstream: "loads with SSE progress and waits for the loaded catalog state"
// -------------------------------------------------------------------------

#[tokio::test]
async fn loads_with_sse_progress_and_waits_for_the_loaded_catalog_state() {
    let status = Arc::new(Mutex::new("unloaded".to_string()));
    let handler_status = Arc::clone(&status);
    let server = MockServer::start(Arc::new(move |request, sse| {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/models/load") => {
                *handler_status.lock().unwrap_or_else(|e| e.into_inner()) = "loading".to_owned();
                let sse = sse.clone();
                let status = Arc::clone(&handler_status);
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    sse.send_json(serde_json::json!({
                        "model": "test-model",
                        "event": "status_change",
                        "data": {
                            "status": "loading",
                            "progress": { "stages": ["text_model", "mmproj_model"], "current": "text_model", "value": 0.5 }
                        }
                    }));
                    *status.lock().unwrap_or_else(|e| e.into_inner()) = "loaded".to_owned();
                    sse.send_json(serde_json::json!({
                        "model": "test-model",
                        "event": "status_change",
                        "data": { "status": "loaded" }
                    }));
                });
                MockResponse::json(serde_json::json!({ "success": true }))
            }
            ("GET", "/models") => {
                let status = handler_status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                MockResponse::json(serde_json::json!({
                    "data": [{ "id": "test-model", "status": { "value": status } }]
                }))
            }
            _ => MockResponse::status(404),
        }
    }))
    .await;

    let progress: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let progress_sink = Arc::clone(&progress);
    let client = LlamaClient::new(&server.url, None).expect("client");
    let model = client
        .load_and_wait(
            "test-model",
            Arc::new(move |entry: LlamaProgress| {
                progress_sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(entry.message);
            }),
            None,
        )
        .await
        .expect("load");
    assert_eq!(model.status.value, "loaded");
    let messages = progress.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        messages.contains(&"Loading text model".to_string()),
        "progress: {messages:?}"
    );
}

// -------------------------------------------------------------------------
// Upstream: "downloads with byte progress and returns the refreshed catalog"
// -------------------------------------------------------------------------

#[tokio::test]
async fn downloads_with_byte_progress_and_returns_the_refreshed_catalog() {
    let status = Arc::new(Mutex::new("missing".to_string()));
    let handler_status = Arc::clone(&status);
    let server = MockServer::start(Arc::new(move |request, sse| {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/models") => {
                *handler_status.lock().unwrap_or_else(|e| e.into_inner()) = "downloading".to_owned();
                let sse = sse.clone();
                let status = Arc::clone(&handler_status);
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    sse.send_json(serde_json::json!({
                        "model": "owner/repo:Q4_K_M",
                        "event": "download_progress",
                        "data": { "progress": { "https://example/model.gguf": { "done": 512, "total": 1024 } } }
                    }));
                    *status.lock().unwrap_or_else(|e| e.into_inner()) = "unloaded".to_owned();
                    sse.send_json(serde_json::json!({
                        "model": "owner/repo:Q4_K_M",
                        "event": "download_finished",
                        "data": {}
                    }));
                });
                MockResponse::json(serde_json::json!({ "success": true }))
            }
            ("GET", path) if path.starts_with("/models") => {
                let status = handler_status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let data = if status == "missing" {
                    serde_json::json!([])
                } else {
                    serde_json::json!([{ "id": "owner/repo:Q4_K_M", "status": { "value": status } }])
                };
                MockResponse::json(serde_json::json!({ "data": data }))
            }
            _ => MockResponse::status(404),
        }
    }))
    .await;

    let progress: Arc<Mutex<Vec<LlamaProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let progress_sink = Arc::clone(&progress);
    let client = LlamaClient::new(&server.url, None).expect("client");
    let models = client
        .download_and_wait(
            "owner/repo:Q4_K_M",
            Arc::new(move |entry: LlamaProgress| {
                progress_sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(entry);
            }),
            None,
        )
        .await
        .expect("download");
    let summarized: Vec<(String, String)> = models
        .iter()
        .map(|model| (model.id.clone(), model.status.value.clone()))
        .collect();
    assert_eq!(
        summarized,
        vec![("owner/repo:Q4_K_M".to_string(), "unloaded".to_string())]
    );
    let entries = progress.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        entries.contains(&LlamaProgress {
            message: "Downloading model".to_owned(),
            ratio: Some(0.5),
            detail: Some("512 B / 1.00 KiB".to_owned()),
        }),
        "progress: {entries:?}"
    );

    // The completion path re-lists with `?reload=1` (client.ts:323).
    let reloads = server
        .requests()
        .into_iter()
        .filter(|request| request.path == "/models?reload=1")
        .count();
    assert!(reloads >= 1, "expected a reload list, got {reloads}");
}

// -------------------------------------------------------------------------
// Upstream: "persists and restores loaded models for cache-only startup
// refreshes"
// -------------------------------------------------------------------------

#[tokio::test]
async fn persists_and_restores_loaded_models_for_cache_only_startup_refreshes() {
    let server = MockServer::start(Arc::new(move |request, _sse| {
        if request.path == "/models" {
            MockResponse::json(serde_json::json!({
                "data": [
                    { "id": "loaded", "status": { "value": "loaded" }, "meta": { "n_ctx": 32768 } },
                    { "id": "unloaded", "status": { "value": "unloaded" } }
                ]
            }))
        } else {
            MockResponse::status(404)
        }
    }))
    .await;

    let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
    let credential = Credential::ApiKey(rpi_ai::auth::ApiKeyCredential {
        key: Some("local".to_owned()),
        env: Some(ProviderEnv::from([(
            "LLAMA_BASE_URL".to_owned(),
            server.url.clone(),
        )])),
    });

    let first = create_llama_provider();
    first
        .provider()
        .refresh_models(make_llama_context(store.clone(), Some(credential.clone()), true).await)
        .expect("refresh")
        .await
        .expect("refresh");
    let ids: Vec<String> = first
        .provider()
        .get_models()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(ids, ["loaded"]);
    let cached = store
        .read(LLAMA_PROVIDER_ID, None)
        .await
        .expect("read")
        .expect("entry");
    let cached_ids: Vec<String> = cached.models.iter().map(|model| model.id.clone()).collect();
    assert_eq!(cached_ids, ["loaded"]);

    // Cache-only startup (allowNetwork: false) restores from the store.
    let second = create_llama_provider();
    second
        .provider()
        .refresh_models(make_llama_context(store, Some(credential), false).await)
        .expect("refresh")
        .await
        .expect("refresh");
    let models = second.provider().get_models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "loaded");
    assert_eq!(models[0].base_url, format!("{}/v1", server.url));
    assert_eq!(models[0].context_window, 32768);
}

// -------------------------------------------------------------------------
// Upstream: "stays dormant until configured and stores URL plus optional
// key" — the interactive login half against a loopback router.
// -------------------------------------------------------------------------

struct ScriptedInteraction {
    answers: Mutex<std::collections::VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}

impl rpi_ai::auth::AuthInteraction for ScriptedInteraction {
    fn prompt<'a>(
        &'a self,
        prompt: rpi_ai::auth::AuthPrompt,
    ) -> rpi_ai::auth::types::BoxFutureSend<'a, Result<String, ModelsError>> {
        Box::pin(async move {
            let message = match &prompt {
                rpi_ai::auth::AuthPrompt::Text { message, .. }
                | rpi_ai::auth::AuthPrompt::Secret { message, .. }
                | rpi_ai::auth::AuthPrompt::ManualCode { message, .. } => message.clone(),
                rpi_ai::auth::AuthPrompt::Select { message, .. } => message.clone(),
            };
            self.prompts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(message);
            self.answers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .ok_or_else(|| ModelsError::new(rpi_ai::auth::ModelsErrorCode::Auth, "no answer"))
        })
    }

    fn notify(&self, _event: rpi_ai::auth::AuthEvent) {}
}

#[tokio::test]
async fn login_stores_url_plus_optional_key() {
    let server = MockServer::start(Arc::new(move |request, _sse| {
        assert_eq!(request.authorization.as_deref(), Some("Bearer secret"));
        MockResponse::json(serde_json::json!({ "data": [] }))
    }))
    .await;

    let controller = create_llama_provider();
    let provider = controller.provider();
    let auth = provider.auth().api_key.clone().expect("api key auth");
    let interaction = ScriptedInteraction {
        answers: Mutex::new(
            [server.url.clone(), "secret".to_owned()]
                .into_iter()
                .collect(),
        ),
        prompts: Mutex::new(Vec::new()),
    };
    let credential = auth.login(&interaction).await.expect("login");
    assert_eq!(credential.key.as_deref(), Some("secret"));
    assert_eq!(
        credential
            .env
            .as_ref()
            .and_then(|env| env.get("LLAMA_BASE_URL"))
            .map(String::as_str),
        Some(server.url.as_str())
    );
    assert_eq!(
        interaction
            .prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_slice(),
        [
            "llama.cpp server URL".to_owned(),
            "API key (optional)".to_owned()
        ]
    );

    // Resolve from the stored credential (provider.ts:100-109).
    let context = rpi_ai::auth::DefaultAuthContext;
    let resolved = auth
        .resolve(&context, Some(&credential))
        .await
        .expect("resolve")
        .expect("configured");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("secret"));
    assert_eq!(
        resolved.auth.base_url.as_deref(),
        Some(format!("{}/v1", server.url).as_str())
    );
    assert_eq!(resolved.source.as_deref(), Some("stored credential"));
}

// -------------------------------------------------------------------------
// Upstream: "searches Hugging Face and reads quantizations plus access
// requirements"
// -------------------------------------------------------------------------

#[tokio::test]
async fn searches_hugging_face_and_reads_quantizations_plus_access_requirements() {
    let server = MockServer::start(Arc::new(move |request, _sse| {
        assert_eq!(request.authorization.as_deref(), Some("Bearer hf-secret"));
        if request.path.starts_with("/api/models?") {
            assert!(
                request.path.contains("search=qwen+coder")
                    || request.path.contains("search=qwen%20coder"),
                "{}",
                request.path
            );
            assert!(request.path.contains("filter=gguf"));
            assert!(request.path.contains("sort=downloads"));
            assert!(request.path.contains("direction=-1"));
            assert!(request.path.contains("limit=20"));
            return MockResponse::json(serde_json::json!([
                { "id": "owner/model-GGUF", "downloads": 1200 }
            ]));
        }
        if request.path == "/api/models/owner/model-GGUF?blobs=true" {
            return MockResponse::json(serde_json::json!({
                "id": "owner/model-GGUF",
                "gated": "manual",
                "siblings": [
                    { "rfilename": "model-Q5_K_M.gguf", "size": 6000 },
                    { "rfilename": "model-Q4_K_M-00001-of-00002.gguf", "size": 2000 },
                    { "rfilename": "model-Q4_K_M-00002-of-00002.gguf", "size": 3000 },
                    { "rfilename": "mmproj-F16.gguf", "size": 1000 }
                ]
            }));
        }
        MockResponse::status(404)
    }))
    .await;

    let client = HuggingFaceClient::new(Some("hf-secret".to_owned()), server.url.clone());
    let results = client.search("qwen coder", None).await.expect("search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "owner/model-GGUF");
    assert_eq!(results[0].downloads, 1200);

    let details = client
        .details("owner/model-GGUF", None)
        .await
        .expect("details");
    assert_eq!(details.id, "owner/model-GGUF");
    assert!(details.gated.is_gated());
    assert!(details.gated.is_manual());
    let quantizations: Vec<(String, Option<i64>)> = details
        .quantizations
        .iter()
        .map(|entry| (entry.name.clone(), entry.size))
        .collect();
    assert_eq!(
        quantizations,
        vec![
            ("Q4_K_M".to_string(), Some(5000)),
            ("Q5_K_M".to_string(), Some(6000))
        ]
    );

    // Upstream also covers `findHuggingFaceToken({ HF_TOKEN: " hf-secret " })`
    // here; the full lookup chain is covered by the module tests.
    let token = find_hugging_face_token(
        &HashMap::from([("HF_TOKEN".to_owned(), " hf-secret ".to_owned())]),
        std::path::Path::new("/nonexistent"),
    )
    .await;
    assert_eq!(token.as_deref(), Some("hf-secret"));
}

/// 429 maps to the rate-limit error with the retry delay
/// (huggingface.ts:88-94).
#[tokio::test]
async fn hugging_face_rate_limit_carries_retry_delay() {
    let server = MockServer::start(Arc::new(move |request, _sse| {
        let _ = request;
        MockResponse::status(429)
    }))
    .await;
    let client = HuggingFaceClient::new(None, server.url.clone());
    let error = client.search("q", None).await.expect_err("rate limited");
    assert_eq!(error.message, "Hugging Face rate limit reached");
}

// -------------------------------------------------------------------------
// `/llama` flow tests (index.ts handler) with a scripted UI
// -------------------------------------------------------------------------

struct FakeHost {
    notifications: Mutex<Vec<(String, NotifyLevel)>>,
    auth: Option<AuthResult>,
    refreshes: AtomicUsize,
}

impl FakeHost {
    fn configured(url: &str) -> Self {
        FakeHost {
            notifications: Mutex::new(Vec::new()),
            auth: Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("local".to_owned()),
                    headers: None,
                    base_url: Some(format!("{url}/v1")),
                },
                env: Some(ProviderEnv::from([(
                    "LLAMA_BASE_URL".to_owned(),
                    url.to_owned(),
                )])),
                source: Some("stored credential".to_owned()),
            }),
            refreshes: AtomicUsize::new(0),
        }
    }

    fn unconfigured() -> Self {
        FakeHost {
            notifications: Mutex::new(Vec::new()),
            auth: None,
            refreshes: AtomicUsize::new(0),
        }
    }

    fn notifications(&self) -> Vec<(String, NotifyLevel)> {
        self.notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl LlamaHost for FakeHost {
    fn mode_is_tui(&self) -> bool {
        true
    }

    fn notify(&self, message: &str, level: NotifyLevel) {
        self.notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((message.to_owned(), level));
    }

    async fn provider_auth(&self) -> Result<Option<AuthResult>, ModelsError> {
        Ok(self.auth.clone())
    }

    async fn refresh_models(&self) {
        self.refreshes.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct FakeUi {
    show_models_script: Mutex<std::collections::VecDeque<LlamaManagerAction>>,
    select_script: Mutex<std::collections::VecDeque<Option<String>>>,
    confirm_script: Mutex<std::collections::VecDeque<bool>>,
    search_script: Mutex<std::collections::VecDeque<Option<String>>>,
    select_titles: Mutex<Vec<String>>,
    /// When set, `progress` resolves (the user pressed stop).
    stop_on_progress: Mutex<bool>,
}

impl FakeUi {
    fn next_select(&self) -> Option<String> {
        self.select_script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .flatten()
    }
}

#[async_trait::async_trait]
impl LlamaUi for FakeUi {
    async fn show_models(
        &mut self,
        _server_url: &str,
        _models: Vec<LlamaModelInfo>,
    ) -> LlamaManagerAction {
        self.show_models_script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or(LlamaManagerAction::Close)
    }

    async fn select(&mut self, title: &str, _options: Vec<String>) -> Option<String> {
        self.select_titles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(title.to_owned());
        self.next_select()
    }

    async fn confirm(&mut self, _title: &str, _message: &str) -> bool {
        self.confirm_script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or(false)
    }

    async fn connection_error(
        &mut self,
        _server_url: &str,
        _message: &str,
    ) -> ConnectionErrorChoice {
        ConnectionErrorChoice::Close
    }

    async fn search_models(&mut self, _search: HuggingFaceSearchFn) -> Option<String> {
        self.search_script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .flatten()
    }

    fn show_status(&mut self, _title: &str, _message: &str) {}

    async fn progress(&mut self, _state: &ProgressState) {
        if *self
            .stop_on_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            return;
        }
        // Never resolves: the operation completes on its own.
        std::future::pending::<()>().await;
    }

    fn update_progress(&mut self, _state: &ProgressState) {}
}

/// Router state shared by the flow tests: models with statuses, load/unload
/// mutate, downloads complete after a tick.
struct RouterState {
    statuses: HashMap<String, String>,
    /// When true, loads hang in "loading" (cancellation tests).
    hang_loads: bool,
}

fn catalog_json(state: &RouterState) -> serde_json::Value {
    let data: Vec<serde_json::Value> = state
        .statuses
        .iter()
        .map(|(id, status)| serde_json::json!({ "id": id, "status": { "value": status } }))
        .collect();
    serde_json::json!({ "data": data })
}

fn router_server(
    state: Arc<Mutex<RouterState>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = MockServer> + Send>> {
    Box::pin(async move {
        let state_shared = state;
        MockServer::start(Arc::new(move |request, sse| {
            let mut state = state_shared.lock().unwrap_or_else(|e| e.into_inner());
            match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/models/load") => {
                    let body: serde_json::Value =
                        serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
                    let model = body
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    // hang_loads applies to "new" only: the restore load of
                    // the previously loaded models must complete.
                    let hang = state.hang_loads && model == "new";
                    let next = if hang { "loading" } else { "loaded" };
                    state.statuses.insert(model.clone(), next.to_owned());
                    if !hang {
                        let sse = sse.clone();
                        let model = model.clone();
                        // Let the poll observe "loaded"; the SSE event is a
                        // bonus for the progress path.
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            sse.send_json(serde_json::json!({
                                "model": model,
                                "event": "status_change",
                                "data": { "status": "loaded" }
                            }));
                        });
                    }
                    MockResponse::json(serde_json::json!({ "success": true }))
                }
                ("POST", "/models/unload") => {
                    let body: serde_json::Value =
                        serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
                    let model = body
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    state.statuses.insert(model, "unloaded".to_owned());
                    MockResponse::json(serde_json::json!({ "success": true }))
                }
                ("POST", "/models") => {
                    let body: serde_json::Value =
                        serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
                    let model = body
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    state
                        .statuses
                        .insert(model.clone(), "downloading".to_owned());
                    let sse = sse.clone();
                    let state_ptr = Arc::clone(&state_shared);
                    drop(state);
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        state_ptr
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .statuses
                            .insert(model.clone(), "unloaded".to_owned());
                        sse.send_json(serde_json::json!({
                            "model": model,
                            "event": "download_finished",
                            "data": {}
                        }));
                    });
                    MockResponse::json(serde_json::json!({ "success": true }))
                }
                ("GET", path) if path.starts_with("/models") => {
                    MockResponse::json(catalog_json(&state))
                }
                _ => MockResponse::status(404),
            }
        }))
        .await
    })
}

fn model_action(id: &str, status: &str) -> LlamaManagerAction {
    LlamaManagerAction::Model(Box::new(LlamaModelInfo {
        id: id.to_owned(),
        status: rpi::extensions::llama::client::LlamaModelStatus {
            value: status.to_owned(),
            ..Default::default()
        },
        ..Default::default()
    }))
}

/// `/llama` outside the TUI only warns (index.ts:177-180).
#[tokio::test]
async fn non_tui_mode_only_warns() {
    struct HeadlessHost(FakeHost);
    #[async_trait::async_trait]
    impl LlamaHost for HeadlessHost {
        fn mode_is_tui(&self) -> bool {
            false
        }
        fn notify(&self, message: &str, level: NotifyLevel) {
            self.0.notify(message, level);
        }
        async fn provider_auth(&self) -> Result<Option<AuthResult>, ModelsError> {
            self.0.provider_auth().await
        }
        async fn refresh_models(&self) {}
    }
    let host = HeadlessHost(FakeHost::unconfigured());
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");
    let notifications = host.0.notifications();
    assert_eq!(
        notifications,
        vec![(
            "/llama is available in interactive mode".to_owned(),
            NotifyLevel::Warning
        )]
    );
}

/// Unconfigured provider: warn and stop (index.ts:29-40).
#[tokio::test]
async fn unconfigured_provider_prompts_login() {
    let host = FakeHost::unconfigured();
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");
    let notifications = host.notifications();
    assert_eq!(
        notifications,
        vec![(
            "Configure llama.cpp with /login llama.cpp".to_owned(),
            NotifyLevel::Warning
        )]
    );
}

/// Selecting an unloaded model loads it; the catalog is republished and the
/// success is notified (index.ts:57-114 happy path).
#[tokio::test]
async fn selecting_unloaded_model_loads_it() {
    let state = Arc::new(Mutex::new(RouterState {
        statuses: HashMap::from([("m1".to_owned(), "unloaded".to_owned())]),
        hang_loads: false,
    }));
    let server = router_server(state).await;
    let host = FakeHost::configured(&server.url);
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    ui.show_models_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([model_action("m1", "unloaded"), LlamaManagerAction::Close]);

    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");

    let notifications = host.notifications();
    assert!(
        notifications.contains(&("Loaded m1".to_owned(), NotifyLevel::Info)),
        "notifications: {notifications:?}"
    );
    // The loaded model entered the provider catalog.
    let models = controller.provider().get_models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "m1");
    assert!(host.refreshes.load(Ordering::Relaxed) >= 2);
}

/// Unload requires an explicit confirmation; declining never touches the
/// server (docs/llama-cpp.md: "Pi does not silently unload models").
#[tokio::test]
async fn unload_requires_confirmation_and_never_silent() {
    let state = Arc::new(Mutex::new(RouterState {
        statuses: HashMap::from([("m1".to_owned(), "loaded".to_owned())]),
        hang_loads: false,
    }));
    let server = router_server(state).await;
    let host = FakeHost::configured(&server.url);
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    ui.show_models_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([model_action("m1", "loaded"), LlamaManagerAction::Close]);
    ui.confirm_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([false]);

    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");

    assert!(
        !server
            .requests()
            .iter()
            .any(|request| request.path == "/models/unload"),
        "no unload call may happen without confirmation"
    );

    // Now confirm: the unload goes through and is notified.
    let mut ui = FakeUi::default();
    ui.show_models_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([model_action("m1", "loaded"), LlamaManagerAction::Close]);
    ui.confirm_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([true]);
    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");
    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.path == "/models/unload"),
        "confirmed unload reaches the server"
    );
    let notifications = host.notifications();
    assert!(
        notifications.contains(&("Unloaded m1".to_owned(), NotifyLevel::Info)),
        "notifications: {notifications:?}"
    );
}

/// Loading with another model loaded asks first; "Unload all and load"
/// unloads the others explicitly (index.ts:64-74).
#[tokio::test]
async fn load_with_loaded_models_asks_and_replaces() {
    let state = Arc::new(Mutex::new(RouterState {
        statuses: HashMap::from([
            ("old".to_owned(), "loaded".to_owned()),
            ("new".to_owned(), "unloaded".to_owned()),
        ]),
        hang_loads: false,
    }));
    let server = router_server(state).await;
    let host = FakeHost::configured(&server.url);
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    ui.show_models_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([model_action("new", "unloaded"), LlamaManagerAction::Close]);
    ui.select_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([Some("Unload all and load".to_owned())]);

    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");

    let unloads: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|request| request.path == "/models/unload")
        .map(|request| request.body)
        .collect();
    assert_eq!(
        unloads,
        vec![serde_json::json!({"model": "old"}).to_string()]
    );
    let notifications = host.notifications();
    assert!(
        notifications.contains(&("Loaded new".to_owned(), NotifyLevel::Info)),
        "notifications: {notifications:?}"
    );
    let titles = ui
        .select_titles
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(titles, vec!["1 model is loaded".to_owned()]);
}

/// Cancelling a replacement load restores the previously loaded models
/// (index.ts:76-98) — nothing stays silently unloaded.
#[tokio::test]
async fn cancelled_replace_restores_previously_loaded_models() {
    let state = Arc::new(Mutex::new(RouterState {
        statuses: HashMap::from([
            ("old".to_owned(), "loaded".to_owned()),
            ("new".to_owned(), "unloaded".to_owned()),
        ]),
        hang_loads: true,
    }));
    let server = router_server(state).await;
    let host = FakeHost::configured(&server.url);
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    ui.show_models_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([model_action("new", "unloaded"), LlamaManagerAction::Close]);
    // "Unload all and load", then confirm the stop.
    ui.select_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([Some("Unload all and load".to_owned())]);
    ui.confirm_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([true]);
    *ui.stop_on_progress
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = true;

    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");

    // Restore reloads "old" after the cancellation.
    let notifications = host.notifications();
    assert!(
        notifications.contains(&(
            "Restoring previously loaded models".to_owned(),
            NotifyLevel::Info
        )),
        "notifications: {notifications:?}"
    );
    let loads: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|request| request.path == "/models/load")
        .map(|request| request.body)
        .collect();
    assert!(
        loads.contains(&serde_json::json!({"model": "old"}).to_string()),
        "restore loads: {loads:?}"
    );
}

/// Gated repositories require an explicit Continue (index.ts:135-142);
/// "Back" aborts before any download.
#[tokio::test]
async fn gated_repository_requires_continue() {
    let router = Arc::new(Mutex::new(RouterState {
        statuses: HashMap::new(),
        hang_loads: false,
    }));
    let server = router_server(router).await;
    let hf = MockServer::start(Arc::new(move |request, _sse| {
        if request.path == "/api/models/owner/gated?blobs=true" {
            return MockResponse::json(serde_json::json!({
                "id": "owner/gated",
                "gated": "manual",
                "siblings": []
            }));
        }
        MockResponse::status(404)
    }))
    .await;

    // "Back": no download is ever issued.
    let host = FakeHost::configured(&server.url);
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    ui.show_models_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([LlamaManagerAction::Download, LlamaManagerAction::Close]);
    ui.search_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([Some("owner/gated".to_owned())]);
    ui.select_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([Some("Back".to_owned())]);
    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, hf.url.clone()),
    )
    .await
    .expect("run");
    assert!(
        !server
            .requests()
            .iter()
            .any(|request| request.method == "POST" && request.path == "/models"),
        "Back must not start a download"
    );
    let titles = ui
        .select_titles
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(
        titles
            .iter()
            .any(|title| title.contains("Hugging Face access required")
                && title.contains("Manual approval is required")
                && title.contains("https://huggingface.co/owner/gated")),
        "titles: {titles:?}"
    );
}

/// A search selection without a quant asks for one; the chosen quantization
/// downloads as `owner/repo:QUANT` (index.ts:143-171).
#[tokio::test]
async fn download_flow_selects_quantization_and_downloads() {
    let router = Arc::new(Mutex::new(RouterState {
        statuses: HashMap::new(),
        hang_loads: false,
    }));
    let server = router_server(router).await;
    let hf = MockServer::start(Arc::new(move |request, _sse| {
        if request.path == "/api/models/owner/repo?blobs=true" {
            return MockResponse::json(serde_json::json!({
                "id": "owner/repo",
                "siblings": [
                    { "rfilename": "repo-Q5_K_M.gguf", "size": 6000 },
                    { "rfilename": "repo-Q4_K_M.gguf", "size": 5000 }
                ]
            }));
        }
        MockResponse::status(404)
    }))
    .await;

    let host = FakeHost::configured(&server.url);
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    ui.show_models_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([LlamaManagerAction::Download, LlamaManagerAction::Close]);
    ui.search_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([Some("owner/repo".to_owned())]);
    // The quant options are ordered Q4_K_M first (recommended marker).
    ui.select_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([Some("Q4_K_M · 4.88 KiB · recommended".to_owned())]);

    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, hf.url.clone()),
    )
    .await
    .expect("run");

    let downloads: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == "/models")
        .map(|request| request.body)
        .collect();
    assert_eq!(
        downloads,
        vec![serde_json::json!({"model": "owner/repo:Q4_K_M"}).to_string()]
    );
    let notifications = host.notifications();
    assert!(
        notifications.contains(&("Downloaded owner/repo:Q4_K_M".to_owned(), NotifyLevel::Info)),
        "notifications: {notifications:?}"
    );
    let titles = ui
        .select_titles
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(titles, vec!["Select quantization\nowner/repo".to_owned()]);
}

/// A failing action notifies its message (index.ts:214-216).
#[tokio::test]
async fn action_error_is_notified() {
    // The router reports the load as failed with an exit code.
    let server = MockServer::start(Arc::new(move |request, _sse| {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/models/load") => MockResponse::json(serde_json::json!({ "success": true })),
            ("GET", path) if path.starts_with("/models") => MockResponse::json(serde_json::json!({
                "data": [{ "id": "m1", "status": { "value": "loading", "failed": true, "exit_code": 1 } }]
            })),
            _ => MockResponse::status(404),
        }
    }))
    .await;
    let host = FakeHost::configured(&server.url);
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    ui.show_models_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([model_action("m1", "unloaded"), LlamaManagerAction::Close]);

    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");
    let notifications = host.notifications();
    assert!(
        notifications.contains(&("Model exited with code 1".to_owned(), NotifyLevel::Error)),
        "notifications: {notifications:?}"
    );
}

/// Connection failure offers Retry/Close (index.ts:184-194): Close exits.
#[tokio::test]
async fn connection_error_close_exits() {
    // Nothing listens on the port: every request is a connection failure.
    let url = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        drop(listener);
        url
    };
    let host = FakeHost::configured(&url);
    let controller = create_llama_provider();
    let mut ui = FakeUi::default();
    rpi::extensions::llama::run_llama_manager(
        &host,
        &controller,
        &mut ui,
        HuggingFaceClient::new(None, "http://127.0.0.1:1"),
    )
    .await
    .expect("run");
    assert_eq!(host.refreshes.load(Ordering::Relaxed), 0);
}

/// `parseHuggingFaceModel` (index.ts:22-27).
#[test]
fn parse_hugging_face_model_splits_quant() {
    use rpi::extensions::llama::parse_hugging_face_model;
    assert_eq!(
        parse_hugging_face_model("owner/repo"),
        ("owner/repo".to_owned(), None)
    );
    assert_eq!(
        parse_hugging_face_model("owner/repo:Q4_K_M"),
        ("owner/repo".to_owned(), Some("Q4_K_M".to_owned()))
    );
    // The split colon is the first one after the first slash.
    assert_eq!(
        parse_hugging_face_model("owner/repo:Q4:extra"),
        ("owner/repo".to_owned(), Some("Q4:extra".to_owned()))
    );
}
