//! Port of `packages/coding-agent/src/extensions/llama/client.ts`
//! @ pi 0.82.1 (2efa728) — llama.cpp router management client: model catalog
//! (`/models`), load/unload/download, SSE progress watch, and the poll-based
//! `loadAndWait`/`downloadAndWait` loops.
//!
//! Intentional differences:
//! - `AbortSignal` becomes [`CancellationToken`]; `sleep(ms, signal)` becomes
//!   [`sleep_or_cancelled`]. Cancellation errors carry the upstream fallback
//!   message `"Cancelled"`.
//! - `fetch` becomes `reqwest` (rustls); the per-request 15s
//!   `AbortSignal.timeout(15_000)` becomes the client-level request timeout.
//! - Connection-failure classification lives on [`LlamaError::connection`]
//!   (reqwest `is_connect`/`is_timeout`); upstream pattern-matches the undici
//!   message text (`fetch failed`/`timeout`/`network`, D-047).

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// `LlamaModelStatus` — the router-reported status block. `value` stays a
/// free-form string (the server may emit values the client predates); the
/// five upstream variants are [`LlamaModelStatusValue`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlamaModelStatus {
    pub value: String,
    pub args: Vec<String>,
    pub failed: bool,
    pub exit_code: Option<i64>,
    /// Download progress map (`Record<string, { done, total }>`), kept raw.
    pub progress: Option<Value>,
}

/// The upstream `LlamaModelStatus` union (client.ts:1).
pub struct LlamaModelStatusValue;

impl LlamaModelStatusValue {
    pub const UNLOADED: &'static str = "unloaded";
    pub const LOADING: &'static str = "loading";
    pub const LOADED: &'static str = "loaded";
    pub const DOWNLOADING: &'static str = "downloading";
    pub const SLEEPING: &'static str = "sleeping";
}

/// `LlamaModelInfo["architecture"]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlamaArchitecture {
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
}

/// `LlamaModelInfo["meta"]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlamaModelMeta {
    pub n_ctx: Option<u64>,
    pub n_ctx_train: Option<u64>,
    pub size: Option<u64>,
    pub ftype: Option<String>,
}

/// `LlamaModelInfo` (client.ts:3-24).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlamaModelInfo {
    pub id: String,
    pub aliases: Vec<String>,
    pub status: LlamaModelStatus,
    pub architecture: Option<LlamaArchitecture>,
    pub source: Option<String>,
    pub meta: Option<LlamaModelMeta>,
}

impl LlamaModelInfo {
    /// `modelIsLoaded` (index.ts:7-9): loaded or sleeping.
    pub fn is_loaded(&self) -> bool {
        self.status.value == LlamaModelStatusValue::LOADED
            || self.status.value == LlamaModelStatusValue::SLEEPING
    }
}

/// `LlamaModelEvent` (client.ts:31-35).
#[derive(Debug, Clone, PartialEq)]
pub struct LlamaModelEvent {
    pub model: String,
    pub event: String,
    pub data: Option<Value>,
}

/// `LlamaProgress` (client.ts:37-41).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlamaProgress {
    pub message: String,
    pub ratio: Option<f64>,
    pub detail: Option<String>,
}

/// llama client error. `connection` marks transport-level failures
/// (connect/timeout) — the `/llama` flow maps those to "Could not connect to
/// the server." (index.ts `isConnectionError`). Never carries credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaError {
    pub message: String,
    pub connection: bool,
}

impl LlamaError {
    pub fn new(message: impl Into<String>) -> Self {
        LlamaError {
            message: message.into(),
            connection: false,
        }
    }

    fn connection(message: impl Into<String>) -> Self {
        LlamaError {
            message: message.into(),
            connection: true,
        }
    }

    fn cancelled() -> Self {
        LlamaError::new("Cancelled")
    }
}

impl std::fmt::Display for LlamaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LlamaError {}

/// `errorMessage(payload, fallback)` (client.ts:43-50): `payload.error.message`.
pub(crate) fn error_message(payload: Option<&Value>, fallback: &str) -> String {
    payload
        .and_then(|payload| payload.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

/// `isModelInfo` (client.ts:51-55): `id` and `status.value` must be strings.
fn is_model_info(value: &Value) -> bool {
    value.get("id").is_some_and(Value::is_string)
        && value
            .get("status")
            .and_then(|status| status.get("value"))
            .is_some_and(Value::is_string)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Lenient field extraction (upstream reads the JSON structurally).
fn parse_model_info(value: &Value) -> LlamaModelInfo {
    let status = value.get("status").cloned().unwrap_or(Value::Null);
    LlamaModelInfo {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        aliases: strings(value.get("aliases")),
        status: LlamaModelStatus {
            value: status
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            args: strings(status.get("args")),
            failed: status
                .get("failed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            exit_code: status.get("exit_code").and_then(Value::as_i64),
            progress: status.get("progress").filter(|p| p.is_object()).cloned(),
        },
        architecture: value
            .get("architecture")
            .map(|architecture| LlamaArchitecture {
                input_modalities: strings(architecture.get("input_modalities")),
                output_modalities: strings(architecture.get("output_modalities")),
            }),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_owned),
        meta: value.get("meta").map(|meta| LlamaModelMeta {
            n_ctx: meta.get("n_ctx").and_then(Value::as_u64),
            n_ctx_train: meta.get("n_ctx_train").and_then(Value::as_u64),
            size: meta.get("size").and_then(Value::as_u64),
            ftype: meta.get("ftype").and_then(Value::as_str).map(str::to_owned),
        }),
    }
}

/// `parseLoadProgress` (client.ts:86-106).
fn parse_load_progress(data: Option<&Value>) -> Option<LlamaProgress> {
    let progress = data?.get("progress")?;
    if !progress.is_object() {
        return None;
    }
    let stage = progress
        .get("current")
        .and_then(Value::as_str)
        .or_else(|| progress.get("stage").and_then(Value::as_str));
    let stages: Vec<&str> = progress
        .get("stages")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let stage_ratio = progress
        .get("value")
        .and_then(Value::as_f64)
        .map(|value| value.clamp(0.0, 1.0));
    let mut ratio = stage_ratio;
    if let (Some(stage), false) = (stage, stages.is_empty()) {
        if let Some(index) = stages.iter().position(|entry| *entry == stage) {
            ratio = Some((index as f64 + stage_ratio.unwrap_or(0.0)) / stages.len() as f64);
        }
    }
    Some(LlamaProgress {
        message: match stage {
            Some(stage) => format!("Loading {}", stage.replace('_', " ")),
            None => "Loading model".to_owned(),
        },
        ratio,
        detail: None,
    })
}

/// `parseDownloadProgress` (client.ts:108-127).
fn parse_download_progress(data: Option<&Value>) -> Option<LlamaProgress> {
    let data = data?;
    let files = match data.get("progress") {
        Some(nested) if nested.is_object() => nested,
        _ => data,
    };
    let mut done = 0.0f64;
    let mut total = 0.0f64;
    for value in files.as_object()?.values() {
        let (Some(entry_done), Some(entry_total)) = (
            value.get("done").and_then(Value::as_f64),
            value.get("total").and_then(Value::as_f64),
        ) else {
            continue;
        };
        done += entry_done;
        total += entry_total;
    }
    if total <= 0.0 {
        return None;
    }
    Some(LlamaProgress {
        message: "Downloading model".to_owned(),
        ratio: Some(done / total),
        detail: Some(format!("{} / {}", format_bytes(done), format_bytes(total))),
    })
}

/// `formatBytes` (client.ts:129-139).
pub fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        return format!("{bytes} B");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes / 1024.0;
    let mut unit = UNITS[0];
    for next in UNITS.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if value >= 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.2} {unit}")
    }
}

/// `normalizeLlamaServerUrl` (client.ts:141-150): http(s) only, fragment and
/// query dropped, trailing slashes and one trailing `/v1` removed from the
/// path, and the trailing slash removed from the serialized URL.
pub fn normalize_llama_server_url(value: &str) -> Result<String, LlamaError> {
    let mut url = url::Url::parse(value.trim()).map_err(|_| LlamaError::new("Invalid URL"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(LlamaError::new("Server URL must use http or https"));
    }
    url.set_fragment(None);
    url.set_query(None);
    let path = url.path().trim_end_matches('/').to_owned();
    let path = path.strip_suffix("/v1").unwrap_or(&path).to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    let serialized = url.to_string();
    Ok(serialized
        .strip_suffix('/')
        .unwrap_or(&serialized)
        .to_owned())
}

/// `llamaInferenceUrl` (client.ts:152-154).
pub fn llama_inference_url(server_url: &str) -> Result<String, LlamaError> {
    Ok(format!("{}/v1", normalize_llama_server_url(server_url)?))
}

/// `sleep(ms, signal)` (client.ts:68-84).
async fn sleep_or_cancelled(
    millis: u64,
    signal: Option<&CancellationToken>,
) -> Result<(), LlamaError> {
    match signal {
        Some(token) => {
            tokio::select! {
                () = token.cancelled() => Err(LlamaError::cancelled()),
                () = tokio::time::sleep(std::time::Duration::from_millis(millis)) => Ok(()),
            }
        }
        None => {
            tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
            Ok(())
        }
    }
}

/// Progress callback shared by `load_and_wait` / `download_and_wait` and
/// their SSE watchers.
pub type LlamaProgressCallback = Arc<dyn Fn(LlamaProgress) + Send + Sync>;

/// Classify a reqwest transport failure (module docs: the connection flag
/// replaces upstream's undici message matching). The message never carries
/// credentials — the API key travels in the Authorization header.
pub(crate) fn transport_error(error: &reqwest::Error) -> LlamaError {
    if error.is_timeout() {
        LlamaError::connection("request timeout")
    } else if error.is_connect() {
        LlamaError::connection(format!("connection failed: {error}"))
    } else {
        LlamaError::new(format!("request failed: {error}"))
    }
}

/// Send honoring the shared cancellation token; a mid-flight cancellation
/// rejects like the upstream `fetch(url, { signal })` AbortError.
pub(crate) async fn send_with_signal(
    request: reqwest::RequestBuilder,
    signal: Option<&CancellationToken>,
) -> Result<reqwest::Response, LlamaError> {
    let send = request.send();
    match signal {
        Some(token) => tokio::select! {
            () = token.cancelled() => Err(LlamaError::cancelled()),
            response = send => response.map_err(|error| transport_error(&error)),
        },
        None => send.await.map_err(|error| transport_error(&error)),
    }
}

/// `LlamaClient` (client.ts:156-332). Two reqwest clients: a 15s-total
/// timeout client for ordinary requests (upstream `AbortSignal.timeout(15000)`
/// on `request()`), and a timeout-free client for the `/models/sse` watch
/// stream — upstream attaches no timeout to `watch()` (client.ts:213-245),
/// and a client-level total timeout would kill long load/download streams
/// at 15s (T14 review M2).
#[derive(Clone)]
pub struct LlamaClient {
    server_url: String,
    api_key: Option<String>,
    /// Per-request 15s timeout (upstream `AbortSignal.timeout(15000)` on
    /// `request()`), applied at the client level.
    http: reqwest::Client,
    /// No-total-timeout client for the SSE stream (`watch()`); long
    /// load/download operations legitimately exceed 15s.
    stream_http: reqwest::Client,
}

impl LlamaClient {
    pub fn new(server_url: &str, api_key: Option<&str>) -> Result<Self, LlamaError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(15_000))
            .build()
            .map_err(|error| LlamaError::new(format!("Failed to build HTTP client: {error}")))?;
        let stream_http = reqwest::Client::builder()
            .build()
            .map_err(|error| LlamaError::new(format!("Failed to build HTTP client: {error}")))?;
        Ok(LlamaClient {
            server_url: normalize_llama_server_url(server_url)?,
            api_key: api_key.filter(|key| !key.is_empty()).map(str::to_owned),
            http,
            stream_http,
        })
    }

    /// The normalized management URL (`client.serverUrl`).
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    /// `request(path, init)` (client.ts:165-180).
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        signal: Option<&CancellationToken>,
    ) -> Result<Option<Value>, LlamaError> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.server_url));
        if body.is_some() {
            request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
        }
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = send_with_signal(request, signal).await?;
        let status = response.status();
        let payload: Option<Value> = response.json().await.ok();
        if !status.is_success() {
            return Err(LlamaError::new(error_message(
                payload.as_ref(),
                &format!("llama.cpp returned HTTP {status}"),
            )));
        }
        Ok(payload)
    }

    /// `list(options)` (client.ts:182-190).
    pub async fn list(
        &self,
        reload: bool,
        signal: Option<&CancellationToken>,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let path = if reload {
            "/models?reload=1"
        } else {
            "/models"
        };
        let payload = self
            .request(reqwest::Method::GET, path, None, signal)
            .await?;
        let data = payload
            .as_ref()
            .and_then(|payload| payload.get("data"))
            .and_then(Value::as_array)
            .ok_or_else(|| LlamaError::new("llama.cpp returned an invalid model catalog"))?;
        if !data.iter().all(is_model_info) {
            return Err(LlamaError::new(
                "Server is not running in llama.cpp router mode",
            ));
        }
        Ok(data.iter().map(parse_model_info).collect())
    }

    /// `load` (client.ts:192-194).
    pub async fn load(
        &self,
        model: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<(), LlamaError> {
        self.request(
            reqwest::Method::POST,
            "/models/load",
            Some(serde_json::json!({ "model": model })),
            signal,
        )
        .await?;
        Ok(())
    }

    /// `unload` (client.ts:196-198).
    pub async fn unload(
        &self,
        model: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<(), LlamaError> {
        self.request(
            reqwest::Method::POST,
            "/models/unload",
            Some(serde_json::json!({ "model": model })),
            signal,
        )
        .await?;
        Ok(())
    }

    /// `unloadAndWait` (client.ts:200-207).
    pub async fn unload_and_wait(
        &self,
        model: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<(), LlamaError> {
        self.unload(model, signal).await?;
        loop {
            let entry = self
                .list(false, signal)
                .await?
                .into_iter()
                .find(|candidate| candidate.id == model);
            match entry {
                None => return Ok(()),
                Some(entry) if entry.status.value == LlamaModelStatusValue::UNLOADED => {
                    return Ok(())
                }
                _ => sleep_or_cancelled(100, signal).await?,
            }
        }
    }

    /// `download` (client.ts:209-211): POST `/models` with `{ model }` — the
    /// llama.cpp server performs the Hugging Face download.
    pub async fn download(
        &self,
        model: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<(), LlamaError> {
        self.request(
            reqwest::Method::POST,
            "/models",
            Some(serde_json::json!({ "model": model })),
            signal,
        )
        .await?;
        Ok(())
    }

    /// `watch(onEvent, signal)` (client.ts:213-245): consume `/models/sse`
    /// until the stream ends or the token cancels. Malformed events are
    /// ignored (catalog polling remains authoritative).
    pub async fn watch<F: FnMut(LlamaModelEvent) + Send>(
        &self,
        on_event: &mut F,
        signal: Option<&CancellationToken>,
    ) -> Result<(), LlamaError> {
        // Timeout-free client (M2): the SSE stream must survive past 15s.
        let mut request = self
            .stream_http
            .get(format!("{}/models/sse", self.server_url));
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }
        let send = request.send();
        let response = match signal {
            Some(token) => tokio::select! {
                () = token.cancelled() => return Err(LlamaError::cancelled()),
                response = send => response.map_err(|error| transport_error(&error))?,
            },
            None => send.await.map_err(|error| transport_error(&error))?,
        };
        if !response.status().is_success() {
            return Err(LlamaError::new(format!(
                "llama.cpp SSE returned HTTP {}",
                response.status()
            )));
        }
        let cancelled = || signal.is_some_and(CancellationToken::is_cancelled);
        let mut buffer: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        loop {
            if cancelled() {
                return Err(LlamaError::cancelled());
            }
            let chunk = match signal {
                Some(token) => {
                    tokio::select! {
                        () = token.cancelled() => return Err(LlamaError::cancelled()),
                        chunk = futures::StreamExt::next(&mut stream) => chunk,
                    }
                }
                None => futures::StreamExt::next(&mut stream).await,
            };
            let Some(chunk) = chunk else {
                return Ok(());
            };
            let chunk = chunk.map_err(|error| transport_error(&error))?;
            // Accumulate raw bytes and decode whole frames only: decoding
            // per chunk could corrupt a multi-byte UTF-8 character split
            // across TCP segments and drop the event (T14 review m1). CR
            // bytes are stripped at the byte level (SSE uses CRLF line
            // endings; a frame boundary is then `\n\n`).
            let chunk = chunk
                .as_ref()
                .iter()
                .copied()
                .filter(|byte| *byte != b'\r')
                .collect::<Vec<u8>>();
            buffer.extend_from_slice(&chunk);
            while let Some(boundary) = buffer.windows(2).position(|window| window == b"\n\n") {
                let frame = buffer[..boundary].to_vec();
                buffer.drain(..boundary + 2);
                let data = String::from_utf8_lossy(&frame)
                    .split('\n')
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                let (Some(model), Some(event_name)) = (
                    event.get("model").and_then(Value::as_str),
                    event.get("event").and_then(Value::as_str),
                ) else {
                    continue;
                };
                on_event(LlamaModelEvent {
                    model: model.to_owned(),
                    event: event_name.to_owned(),
                    data: event.get("data").cloned(),
                });
            }
        }
    }

    /// `loadAndWait` (client.ts:247-286).
    /// `loadAndWait` (client.ts:247-286): start an SSE watcher, issue the
    /// load, then poll the catalog until the model reports `loaded` (or the
    /// watcher saw it loaded and the entry already dropped out). Watcher
    /// errors are swallowed (`void … .catch(() => {})`) — polling stays
    /// authoritative.
    pub async fn load_and_wait(
        &self,
        model: &str,
        on_progress: LlamaProgressCallback,
        signal: Option<&CancellationToken>,
    ) -> Result<LlamaModelInfo, LlamaError> {
        let state = Arc::new(Mutex::new(LoadWatchState::default()));
        let watcher = CancellationToken::new();
        let watch_handle = {
            let state = Arc::clone(&state);
            let watcher = watcher.clone();
            let progress = Arc::clone(&on_progress);
            let model = model.to_owned();
            // Reuse this client (incl. its timeout-free SSE client) instead
            // of rebuilding one per operation (T14 review m4).
            let client = self.clone();
            tokio::spawn(async move {
                let _ = client
                    .watch(
                        &mut |event: LlamaModelEvent| {
                            if event.model != model {
                                return;
                            }
                            if event.event != "model_status" && event.event != "status_change" {
                                return;
                            }
                            let status = event
                                .data
                                .as_ref()
                                .and_then(|data| data.get("status"))
                                .and_then(Value::as_str);
                            if status == Some(LlamaModelStatusValue::LOADED) {
                                state.lock().unwrap_or_else(|e| e.into_inner()).event_loaded = true;
                            }
                            if status == Some(LlamaModelStatusValue::UNLOADED) {
                                state.lock().unwrap_or_else(|e| e.into_inner()).event_error =
                                    Some("Model failed to load".to_owned());
                            }
                            if let Some(progress_value) = parse_load_progress(event.data.as_ref()) {
                                progress(progress_value);
                            }
                        },
                        Some(&watcher),
                    )
                    .await;
            })
        };
        let result = self
            .load_and_wait_inner(model, &on_progress, &state, signal)
            .await;
        watcher.cancel();
        watch_handle.abort();
        result
    }

    async fn load_and_wait_inner(
        &self,
        model: &str,
        on_progress: &LlamaProgressCallback,
        state: &Mutex<LoadWatchState>,
        signal: Option<&CancellationToken>,
    ) -> Result<LlamaModelInfo, LlamaError> {
        self.load(model, signal).await?;
        on_progress(LlamaProgress {
            message: "Loading model".to_owned(),
            ratio: None,
            detail: None,
        });
        loop {
            if signal.is_some_and(CancellationToken::is_cancelled) {
                return Err(LlamaError::cancelled());
            }
            let entry = self
                .list(false, signal)
                .await?
                .into_iter()
                .find(|candidate| candidate.id == model);
            if let Some(entry) = &entry {
                if entry.status.value == LlamaModelStatusValue::LOADED {
                    return Ok(entry.clone());
                }
            }
            let (event_loaded, event_error) = {
                let state = state.lock().unwrap_or_else(|e| e.into_inner());
                (state.event_loaded, state.event_error.clone())
            };
            if entry.is_none() && event_loaded {
                return Ok(LlamaModelInfo {
                    id: model.to_owned(),
                    status: LlamaModelStatus {
                        value: LlamaModelStatusValue::LOADED.to_owned(),
                        ..LlamaModelStatus::default()
                    },
                    ..LlamaModelInfo::default()
                });
            }
            let failed = entry.as_ref().is_some_and(|entry| entry.status.failed);
            if failed || event_error.is_some() {
                let exit_code = entry.as_ref().and_then(|entry| entry.status.exit_code);
                return Err(LlamaError::new(match exit_code {
                    Some(code) => format!("Model exited with code {code}"),
                    None => event_error.unwrap_or_else(|| "Model failed to load".to_owned()),
                }));
            }
            sleep_or_cancelled(250, signal).await?;
        }
    }

    /// `downloadAndWait` (client.ts:288-331): resolves with the reloaded
    /// catalog (`list({ reload: true })`).
    pub async fn download_and_wait(
        &self,
        model: &str,
        on_progress: LlamaProgressCallback,
        signal: Option<&CancellationToken>,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let state = Arc::new(Mutex::new(DownloadWatchState::default()));
        let watcher = CancellationToken::new();
        let watch_handle = {
            let state = Arc::clone(&state);
            let watcher = watcher.clone();
            let progress = Arc::clone(&on_progress);
            let model = model.to_owned();
            // Reuse this client (incl. its timeout-free SSE client) instead
            // of rebuilding one per operation (T14 review m4).
            let client = self.clone();
            tokio::spawn(async move {
                let _ = client
                    .watch(
                        &mut |event: LlamaModelEvent| {
                            if event.model != model {
                                return;
                            }
                            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                            if event.event == "download_finished" {
                                state.finished = true;
                            }
                            if event.event == "download_failed" {
                                state.failure =
                                    Some(error_message(event.data.as_ref(), "Download failed"));
                            }
                            if event.event == "download_progress" {
                                state.saw_downloading = true;
                                drop(state);
                                if let Some(progress_value) =
                                    parse_download_progress(event.data.as_ref())
                                {
                                    progress(progress_value);
                                }
                            }
                        },
                        Some(&watcher),
                    )
                    .await;
            })
        };
        let result = self
            .download_and_wait_inner(model, &on_progress, &state, signal)
            .await;
        watcher.cancel();
        watch_handle.abort();
        result
    }

    async fn download_and_wait_inner(
        &self,
        model: &str,
        on_progress: &LlamaProgressCallback,
        state: &Mutex<DownloadWatchState>,
        signal: Option<&CancellationToken>,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        self.download(model, signal).await?;
        on_progress(LlamaProgress {
            message: "Downloading model".to_owned(),
            ratio: None,
            detail: None,
        });
        let mut polls = 0u32;
        loop {
            if signal.is_some_and(CancellationToken::is_cancelled) {
                return Err(LlamaError::cancelled());
            }
            let (finished, failure, saw_downloading) = {
                let state = state.lock().unwrap_or_else(|e| e.into_inner());
                (state.finished, state.failure.clone(), state.saw_downloading)
            };
            if let Some(failure) = failure {
                return Err(LlamaError::new(failure));
            }
            let models = self.list(false, signal).await?;
            polls += 1;
            let entry = models.iter().find(|candidate| candidate.id == model);
            if entry
                .as_ref()
                .is_some_and(|entry| entry.status.value == LlamaModelStatusValue::DOWNLOADING)
            {
                state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .saw_downloading = true;
                if let Some(progress_value) = entry
                    .and_then(|entry| entry.status.progress.as_ref())
                    .and_then(|progress| parse_download_progress(Some(progress)))
                {
                    on_progress(progress_value);
                }
            } else if finished || (entry.is_some() && (saw_downloading || polls >= 2)) {
                return self.list(true, signal).await;
            }
            sleep_or_cancelled(500, signal).await?;
        }
    }
}

/// Shared state of the `load_and_wait` SSE watcher (client.ts:252-264).
#[derive(Default)]
struct LoadWatchState {
    event_loaded: bool,
    event_error: Option<String>,
}

/// Shared state of the `download_and_wait` SSE watcher (client.ts:294-308).
#[derive(Default)]
struct DownloadWatchState {
    finished: bool,
    failure: Option<String>,
    saw_downloading: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream: "normalizes management and inference URLs"
    /// (llama-extension.test.ts).
    #[test]
    fn normalizes_management_and_inference_urls() {
        assert_eq!(
            normalize_llama_server_url("http://127.0.0.1:8080/v1/").as_deref(),
            Ok("http://127.0.0.1:8080")
        );
        assert_eq!(
            normalize_llama_server_url("https://example.com/prefix/v1").as_deref(),
            Ok("https://example.com/prefix")
        );
        let error = normalize_llama_server_url("file:///tmp/llama").expect_err("file scheme");
        assert!(
            error.message.contains("http or https"),
            "message: {}",
            error.message
        );
        assert_eq!(
            llama_inference_url("http://127.0.0.1:8080/").as_deref(),
            Ok("http://127.0.0.1:8080/v1")
        );
    }

    /// `formatBytes` boundaries (client.ts:129-139).
    #[test]
    fn format_bytes_thresholds_and_precision() {
        assert_eq!(format_bytes(0.0), "0 B");
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(1023.0), "1023 B");
        assert_eq!(format_bytes(1024.0), "1.00 KiB");
        assert_eq!(format_bytes(1536.0), "1.50 KiB");
        assert_eq!(format_bytes(10.0 * 1024.0), "10.0 KiB");
        assert_eq!(format_bytes(1024.0 * 1024.0), "1.00 MiB");
        assert_eq!(format_bytes(1024.0 * 1024.0 * 1024.0), "1.00 GiB");
    }

    /// `parseLoadProgress` (client.ts:86-106): stage ratio averaged over the
    /// stage list, underscores rendered as spaces.
    #[test]
    fn parses_load_progress_with_stage_ratio() {
        let data = serde_json::json!({
            "status": "loading",
            "progress": { "stages": ["text_model", "mmproj_model"], "current": "text_model", "value": 0.5 }
        });
        let progress = parse_load_progress(Some(&data)).expect("progress");
        assert_eq!(progress.message, "Loading text model");
        assert_eq!(progress.ratio, Some(0.25));
        assert_eq!(progress.detail, None);

        let bare = serde_json::json!({ "progress": { "value": 2.0 } });
        let progress = parse_load_progress(Some(&bare)).expect("progress");
        assert_eq!(progress.message, "Loading model");
        assert_eq!(progress.ratio, Some(1.0));

        assert_eq!(parse_load_progress(Some(&serde_json::json!({}))), None);
        assert_eq!(parse_load_progress(None), None);
    }

    /// `parseDownloadProgress` (client.ts:108-127): byte sums over the file
    /// map, `data.progress` unwrapped when present.
    #[test]
    fn parses_download_progress_byte_sums() {
        let data = serde_json::json!({
            "progress": { "https://example/model.gguf": { "done": 512, "total": 1024 } }
        });
        let progress = parse_download_progress(Some(&data)).expect("progress");
        assert_eq!(progress.message, "Downloading model");
        assert_eq!(progress.ratio, Some(0.5));
        assert_eq!(progress.detail.as_deref(), Some("512 B / 1.00 KiB"));

        // No `progress` wrapper: the data object itself is the file map.
        let flat =
            serde_json::json!({ "a": { "done": 1, "total": 4 }, "b": { "done": 1, "total": 4 } });
        let progress = parse_download_progress(Some(&flat)).expect("progress");
        assert_eq!(progress.ratio, Some(0.25));

        assert_eq!(
            parse_download_progress(Some(&serde_json::json!({ "a": { "done": 1 } }))),
            None
        );
    }

    /// `isModelInfo` drives the router-mode validation error (client.ts:
    /// 187-189).
    #[test]
    fn model_info_validation() {
        assert!(is_model_info(
            &serde_json::json!({ "id": "m", "status": { "value": "loaded" } })
        ));
        assert!(!is_model_info(&serde_json::json!({ "id": "m" })));
        assert!(!is_model_info(
            &serde_json::json!({ "status": { "value": "loaded" } })
        ));
    }
}
