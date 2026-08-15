//! Batch fetch surface: response-text rendering, tool-default resolution and
//! the bounded-concurrency worker pool — ports of upstream `tool.ts`
//! (defaults + `executeBatchFetchToolCall` tool.ts:218-348) and the batch
//! half of `format.ts` @ b0111612 (FR-P1-1).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::constants::{
    DEFAULT_BATCH_CONCURRENCY, DEFAULT_BROWSER, DEFAULT_MAX_CHARS, DEFAULT_OS, DEFAULT_TIMEOUT_MS,
};
use crate::format::{
    build_fetch_error_response_text, build_fetch_response_text, build_header,
    build_user_facing_fetch_error_summary, HeaderValue,
};
use crate::pipeline::{FetchExecutionHooks, FetchPipeline};
use crate::types::{
    FetchOptions, FetchOutcome, FetchResult, FetchToolConfig, FetchToolDefaults, IncludeReplies,
    OutputFormat, WebFetchParams,
};

/// `resolveBatchConcurrency` (tool.ts:29-35): non-finite/0 → default; floor
/// with a lower bound of 1.
pub fn resolve_batch_concurrency(value: Option<f64>) -> u64 {
    match value {
        Some(value) if value.is_finite() && value != 0.0 => std::cmp::max(1, value.floor() as u64),
        _ => DEFAULT_BATCH_CONCURRENCY,
    }
}

/// `resolveFetchToolDefaults` (tool.ts:37-50).
pub fn resolve_fetch_tool_defaults(config: &FetchToolConfig) -> FetchToolDefaults {
    FetchToolDefaults {
        max_chars: config.max_chars.unwrap_or(DEFAULT_MAX_CHARS),
        timeout_ms: config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        browser: config
            .browser
            .clone()
            .unwrap_or_else(|| DEFAULT_BROWSER.to_string()),
        os: config.os.clone().unwrap_or_else(|| DEFAULT_OS.to_string()),
        remove_images: config.remove_images.unwrap_or(false),
        include_replies: config
            .include_replies
            .clone()
            .unwrap_or(IncludeReplies::Extractors),
        batch_concurrency: resolve_batch_concurrency(config.batch_concurrency),
        temp_dir: config.temp_dir.clone(),
    }
}

/// `buildFetchOptionsFromParams` (tool.ts:135-155): tool params + defaults →
/// the pipeline's FetchOptions.
pub fn build_fetch_options_from_params(
    params: &crate::types::WebFetchParams,
    defaults: &FetchToolDefaults,
) -> FetchOptions {
    FetchOptions {
        url: params.url.clone(),
        browser: Some(
            params
                .browser
                .clone()
                .unwrap_or_else(|| defaults.browser.clone()),
        ),
        os: Some(params.os.clone().unwrap_or_else(|| defaults.os.clone())),
        headers: params.headers.clone(),
        max_chars: Some(params.max_chars.unwrap_or(defaults.max_chars)),
        format: Some(
            params
                .format
                .map(crate::types::OutputFormat::from)
                .unwrap_or(OutputFormat::Markdown),
        ),
        remove_images: Some(params.remove_images.unwrap_or(defaults.remove_images)),
        include_replies: Some(
            params
                .include_replies
                .clone()
                .unwrap_or_else(|| defaults.include_replies.clone()),
        ),
        proxy: params.proxy.clone(),
        timeout_ms: Some(params.timeout_ms.unwrap_or(defaults.timeout_ms)),
        temp_dir: defaults.temp_dir.clone(),
    }
}

/// `BatchFetchItemResult` (types.ts:100-107) — error items carry `error`
/// text; success items carry the result. `request` is the normalized options
/// (`buildFetchOptionsFromParams` output) the item ran with — upstream keeps
/// it on the item for the details payload.
#[derive(Debug, Clone)]
pub struct BatchFetchItemResult {
    pub index: usize,
    pub request_url: String,
    pub status: &'static str, // "done" | "error"
    pub error: Option<String>,
    pub result: Option<FetchResult>,
    /// Normalized options (serialized into the details payload only).
    pub request: Option<FetchOptions>,
}

/// `BatchFetchResult` (types.ts:118-124).
#[derive(Debug, Clone)]
pub struct BatchFetchResult {
    pub items: Vec<BatchFetchItemResult>,
    pub total: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub batch_concurrency: u64,
}

/// `buildBatchItemHeading` (format.ts:313-320).
fn build_batch_item_heading(item: &BatchFetchItemResult, total: u64) -> String {
    let ordinal = item.index + 1;
    let url = item
        .result
        .as_ref()
        .map(|result| result.final_url.clone())
        .unwrap_or_else(|| item.request_url.clone());
    format!("## [{ordinal}/{total}] {url}")
}

/// `buildBatchItemText` (format.ts:322-344).
fn build_batch_item_text(item: &BatchFetchItemResult, total: u64, verbose: bool) -> String {
    let heading = build_batch_item_heading(item, total);
    if item.status == "error" {
        let error_text = item
            .error
            .clone()
            .unwrap_or_else(|| "Unknown error".to_string());
        if error_text.contains('\n') {
            return format!("{heading}\n{error_text}");
        }
        // strip the "Error: " prefix the single-fetch text already carries
        let stripped = error_text
            .strip_prefix("Error:")
            .map(|rest| rest.trim_start())
            .unwrap_or(&error_text)
            .to_string();
        let error_header = build_header(&[
            ("URL", HeaderValue::from(item.request_url.as_str())),
            ("Status", HeaderValue::from("error")),
            ("Error", HeaderValue::from(stripped)),
        ]);
        return format!("{heading}\n{error_header}");
    }
    let result = item.result.as_ref().expect("done items carry a result");
    format!("{heading}\n{}", build_fetch_response_text(result, verbose))
}

/// `buildBatchFetchResponseText` (format.ts:346-361).
pub fn build_batch_fetch_response_text(result: &BatchFetchResult, verbose: bool) -> String {
    let summary = build_header(&[
        ("Requests", HeaderValue::from(result.total)),
        ("Succeeded", HeaderValue::from(result.succeeded)),
        ("Failed", HeaderValue::from(result.failed)),
        ("Concurrency", HeaderValue::from(result.batch_concurrency)),
    ]);
    let items = result
        .items
        .iter()
        .map(|item| build_batch_item_text(item, result.total, verbose))
        .collect::<Vec<_>>();
    let mut sections = vec![summary];
    sections.extend(items);
    sections
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Convenience for the worker pool: fold one pipeline outcome into a batch
/// item (error items carry `buildFetchErrorResponseText` — the same text
/// `executeBatchFetchToolCall` records, tool.ts:296-304).
pub fn batch_item_from_outcome(
    index: usize,
    request: &FetchOptions,
    outcome: FetchOutcome,
) -> BatchFetchItemResult {
    let request_url = request.url.clone();
    let normalized = Some(request.clone());
    match outcome {
        FetchOutcome::Result(result) => BatchFetchItemResult {
            index,
            request_url,
            status: "done",
            error: None,
            result: Some(result),
            request: normalized,
        },
        FetchOutcome::Error(error) => BatchFetchItemResult {
            index,
            request_url,
            status: "error",
            error: Some(build_fetch_error_response_text(&error)),
            result: None,
            request: normalized,
        },
    }
}

/// A batch request whose params failed to deserialize: upstream's TypeBox
/// layer rejects the whole call before execute, so there is no per-item
/// fixture — the Rust side degrades to the single-fetch catch-all template
/// (index.ts:604-606) as the item error.
pub fn batch_item_from_parse_failure(
    index: usize,
    url_hint: &str,
    message: &str,
) -> BatchFetchItemResult {
    BatchFetchItemResult {
        index,
        request_url: url_hint.to_string(),
        status: "error",
        error: Some(format!(
            "Error: Unexpected web_fetch failure for {url_hint}.\n\n{message}"
        )),
        result: None,
        request: None,
    }
}

// ===== batch progress stream (FR-P2-C, tool.ts:165-216) =====

/// `PROGRESS_BY_STATUS` (tool.ts:165-173) — done/error both render 1; the
/// intermediate states never appear in a final snapshot.
pub fn progress_by_status(status: &str) -> f64 {
    match status {
        "waiting" => 0.11,
        "loading" => 0.51,
        "processing" => 0.96,
        "done" | "error" => 1.0,
        // queued / connecting / unknown
        _ => 0.0,
    }
}

/// `BatchFetchItemProgress` (types.ts:91-98).
#[derive(Debug, Clone)]
pub struct BatchFetchItemProgress {
    pub index: usize,
    pub url: String,
    pub status: String,
    pub progress: f64,
    pub status_started_at: Option<u64>,
    pub error: Option<String>,
}

/// `BatchFetchProgressSnapshot` (types.ts:109-116) — shallow-copied item
/// list plus the derived counters (`buildProgressSnapshot`, tool.ts:188-216).
#[derive(Debug, Clone)]
pub struct BatchFetchProgressSnapshot {
    pub items: Vec<BatchFetchItemProgress>,
    pub total: u64,
    pub completed: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub batch_concurrency: u64,
}

/// JSON number normalization: JS `JSON.stringify(0)` renders `0`, serde
/// would render `0.0` for f64 — integral values serialize as integers so
/// both sides compare equal.
pub fn progress_json(value: f64) -> serde_json::Value {
    if value.fract() == 0.0 && value.is_finite() {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn progress_item_json(item: &BatchFetchItemProgress) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("index".to_string(), json!(item.index));
    map.insert("url".to_string(), json!(item.url));
    map.insert("status".to_string(), json!(item.status));
    map.insert("progress".to_string(), progress_json(item.progress));
    if let Some(started_at) = item.status_started_at {
        map.insert("statusStartedAt".to_string(), json!(started_at));
    }
    if let Some(error) = &item.error {
        map.insert("error".to_string(), json!(error));
    }
    serde_json::Value::Object(map)
}

impl BatchFetchProgressSnapshot {
    /// The details payload shape (index.ts:701-707 rides this JSON under
    /// `details.batchProgress`).
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "items": self.items.iter().map(progress_item_json).collect::<Vec<_>>(),
            "total": self.total,
            "completed": self.completed,
            "succeeded": self.succeeded,
            "failed": self.failed,
            "batchConcurrency": self.batch_concurrency,
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// `createInitialProgressItems` (tool.ts:175-186) — the raw request array
/// (JSON values, pre-parse) so malformed `url` fields render like upstream's
/// `String(request.url ?? "")`.
fn create_initial_progress_items(entries: &[serde_json::Value]) -> Vec<BatchFetchItemProgress> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| BatchFetchItemProgress {
            index,
            url: js_template_render(entry.get("url").unwrap_or(&serde_json::Value::Null)),
            status: "queued".to_string(),
            progress: progress_by_status("queued"),
            status_started_at: Some(now_ms()),
            error: None,
        })
        .collect()
}

/// `buildProgressSnapshot` (tool.ts:188-216).
fn build_progress_snapshot(
    items: &[BatchFetchItemProgress],
    batch_concurrency: u64,
) -> BatchFetchProgressSnapshot {
    let mut completed = 0u64;
    let mut succeeded = 0u64;
    let mut failed = 0u64;
    for item in items {
        match item.status.as_str() {
            "done" => {
                completed += 1;
                succeeded += 1;
            }
            "error" => {
                completed += 1;
                failed += 1;
            }
            _ => {}
        }
    }
    BatchFetchProgressSnapshot {
        items: items.to_vec(),
        total: items.len() as u64,
        completed,
        succeeded,
        failed,
        batch_concurrency,
    }
}

/// The shared progress table + snapshot sink (`progressItems` +
/// `emitProgress`/`updateProgress`, tool.ts:237-265). The emit callback runs
/// OUTSIDE the table lock (coding-standards §6.5 — the execute-layer sink
/// makes a synchronous FFI `toolUpdate` call).
pub type BatchProgressSink = Arc<dyn Fn(&BatchFetchProgressSnapshot) + Send + Sync>;

#[derive(Clone, Default)]
pub struct BatchProgressState {
    table: Arc<Mutex<Vec<BatchFetchItemProgress>>>,
    batch_concurrency: Arc<u64>,
    sink: Option<BatchProgressSink>,
}

impl BatchProgressState {
    fn new(
        items: Vec<BatchFetchItemProgress>,
        batch_concurrency: u64,
        sink: Option<BatchProgressSink>,
    ) -> Self {
        BatchProgressState {
            table: Arc::new(Mutex::new(items)),
            batch_concurrency: Arc::new(batch_concurrency),
            sink,
        }
    }

    /// The pre-worker initial frame (tool.ts:267) — all items queued.
    fn emit_initial(&self) {
        self.emit();
    }

    fn emit(&self) {
        let Some(sink) = &self.sink else {
            return;
        };
        let snapshot = {
            let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
            build_progress_snapshot(&table, *self.batch_concurrency)
        };
        sink(&snapshot);
    }

    /// `updateProgress` (tool.ts:246-265): status change, progress from the
    /// table unless overridden (clamped), `statusStartedAt` kept when the
    /// status is unchanged, error only set (never cleared).
    fn update(&self, index: usize, status: &str, error: Option<String>, progress: Option<f64>) {
        {
            let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
            let Some(item) = table.get_mut(index) else {
                return;
            };
            let status_unchanged = item.status == status;
            item.status = status.to_string();
            item.progress = match progress {
                Some(value) => value.clamp(0.0, 1.0),
                None => progress_by_status(status),
            };
            if !status_unchanged {
                item.status_started_at = Some(now_ms());
            }
            if let Some(error) = error {
                item.error = Some(error);
            }
        }
        self.emit();
    }

    /// `buildProgressSnapshot` over the live table (the execute layer reads
    /// the latest state for spinner frames).
    pub fn snapshot(&self) -> Option<BatchFetchProgressSnapshot> {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        Some(build_progress_snapshot(&table, *self.batch_concurrency))
    }
}

/// `executeBatchFetchToolCall` (tool.ts:218-348): a worker pool of
/// `min(concurrency, len)` claim-next workers — bounded in-flight requests,
/// per-item error isolation, results kept in input order (each worker writes
/// by index). FR-P2-C: the per-item `FetchExecutionHooks` bridge into the
/// shared progress table and every change emits a snapshot frame (the
/// execute layer streams them through `toolUpdate`).
pub async fn execute_batch_fetch_with_progress(
    pipeline: &FetchPipeline,
    requests: &[WebFetchParams],
    defaults: &FetchToolDefaults,
    progress: Option<BatchProgressState>,
    index_map: &[usize],
) -> BatchFetchResult {
    let worker_count = if requests.is_empty() {
        0
    } else {
        std::cmp::min(defaults.batch_concurrency as usize, requests.len())
    };
    let results: Arc<Mutex<Vec<Option<BatchFetchItemResult>>>> =
        Arc::new(Mutex::new((0..requests.len()).map(|_| None).collect()));
    let next_index = Arc::new(AtomicUsize::new(0));

    // NOTE: the initial all-queued frame is the CALLER's (tool.ts:267 —
    // `executeBatchFetchToolCall` emits once before the workers start);
    // `execute_batch_entries` owns it here.

    // Upstream's `nextIndex` claim loop (tool.ts:269-333), one async worker
    // per slot polled concurrently — the single-threaded event-loop
    // concurrency profile of `Promise.all(Array.from(..., worker))`.
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let results = Arc::clone(&results);
        let next_index = Arc::clone(&next_index);
        let progress = progress.clone();
        // Borrows pipeline/requests/defaults outlive the join below.
        workers.push(async move {
            loop {
                let index = next_index.fetch_add(1, Ordering::SeqCst);
                if index >= requests.len() {
                    return;
                }
                // Progress-table coordinates are the INPUT positions; the
                // pool only carries the well-formed requests, so its claim
                // index maps back through `index_map` (identity for the
                // parse-clean call path).
                let global_index = index_map[index];
                let request = &requests[index];
                let opts = build_fetch_options_from_params(request, defaults);
                // The per-item hooks (tool.ts:285-294): `done` never reaches
                // the table through the hooks (the terminal frame lands
                // after the outcome is recorded below).
                let hooks = match &progress {
                    Some(progress) => {
                        let on_status = {
                            let progress = progress.clone();
                            move |status: &str| {
                                if status == "done" {
                                    return;
                                }
                                progress.update(global_index, status, None, None);
                            }
                        };
                        let on_progress = {
                            let progress = progress.clone();
                            move |update: &crate::pipeline::FetchProgressUpdate| {
                                if update.status == "done" {
                                    return;
                                }
                                progress.update(
                                    global_index,
                                    update.status,
                                    None,
                                    Some(update.progress),
                                );
                            }
                        };
                        FetchExecutionHooks {
                            on_status_change: Some(Arc::new(on_status)),
                            on_progress_change: Some(Arc::new(on_progress)),
                        }
                    }
                    None => FetchExecutionHooks::default(),
                };
                let outcome = pipeline.fetch_with_hooks(&opts, &hooks).await;
                // tool.ts:296-320: `results[index]` lands first, then the
                // terminal progress frame; error items carry the user-facing
                // summary in the progress table (the full error text rides
                // the result item).
                let terminal_frame = match &outcome {
                    FetchOutcome::Error(error) => {
                        ("error", Some(build_user_facing_fetch_error_summary(error)))
                    }
                    FetchOutcome::Result(_) => ("done", None),
                };
                let item = batch_item_from_outcome(index, &opts, outcome);
                // recover a poisoned lock (the critical section cannot panic;
                // every request must land its item or the count invariant
                // downstream breaks)
                let mut slot = results.lock().unwrap_or_else(|error| error.into_inner());
                slot[index] = Some(item);
                drop(slot);
                if let Some(progress) = &progress {
                    progress.update(global_index, terminal_frame.0, terminal_frame.1, None);
                }
            }
        });
    }
    futures::future::join_all(workers).await;

    let items: Vec<BatchFetchItemResult> = Arc::try_unwrap(results)
        .map(|mutex| {
            mutex
                .into_inner()
                .unwrap_or_else(|error| error.into_inner())
                .into_iter()
                .flatten()
                .collect()
        })
        .unwrap_or_default();
    debug_assert_eq!(
        items.len(),
        requests.len(),
        "every request resolves to an item"
    );

    let succeeded = items.iter().filter(|item| item.status == "done").count() as u64;
    let failed = items.iter().filter(|item| item.status == "error").count() as u64;
    BatchFetchResult {
        total: requests.len() as u64,
        succeeded,
        failed,
        items,
        batch_concurrency: defaults.batch_concurrency,
    }
}

/// `executeBatchFetchToolCall` without a progress sink (mock-server compat).
pub async fn execute_batch_fetch(
    pipeline: &FetchPipeline,
    requests: &[WebFetchParams],
    defaults: &FetchToolDefaults,
) -> BatchFetchResult {
    let index_map: Vec<usize> = (0..requests.len()).collect();
    execute_batch_fetch_with_progress(pipeline, requests, defaults, None, &index_map).await
}

/// The batch tool payload (index.ts:673-681): `requests` + `verbose`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BatchWebFetchParams {
    pub requests: Vec<serde_json::Value>,
    #[serde(default)]
    pub verbose: Option<bool>,
}

// ===== details payload (index.ts:734-764) =====

/// `PROGRESS_BY_STATUS` (tool.ts:165-173) — done/error both render 1; the
/// intermediate states never appear in a final snapshot.
const PROGRESS_DONE: f64 = 1.0;

fn insert_some(
    map: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<serde_json::Value>,
) {
    // JS `JSON.stringify` drops undefined keys — None entries stay absent.
    if let Some(value) = value {
        map.insert(key.to_string(), value);
    }
}

/// The normalized options rendered like upstream's `request` field
/// (`buildFetchOptionsFromParams` output).
pub fn fetch_options_json(opts: &FetchOptions) -> serde_json::Value {
    use serde_json::Value;
    let mut map = serde_json::Map::new();
    map.insert("url".to_string(), Value::String(opts.url.clone()));
    insert_some(&mut map, "browser", opts.browser.clone().map(Value::String));
    insert_some(&mut map, "os", opts.os.clone().map(Value::String));
    insert_some(
        &mut map,
        "headers",
        opts.headers
            .as_ref()
            .map(|headers| serde_json::to_value(headers).unwrap_or(Value::Null)),
    );
    insert_some(
        &mut map,
        "maxChars",
        opts.max_chars.map(|value| json!(value)),
    );
    insert_some(
        &mut map,
        "format",
        opts.format.map(|format| json!(format.as_str())),
    );
    insert_some(
        &mut map,
        "removeImages",
        opts.remove_images.map(|value| json!(value)),
    );
    insert_some(
        &mut map,
        "includeReplies",
        opts.include_replies.as_ref().map(|value| {
            json!(match value {
                IncludeReplies::All => serde_json::Value::Bool(true),
                IncludeReplies::None => serde_json::Value::Bool(false),
                IncludeReplies::Extractors => {
                    serde_json::Value::String("extractors".to_string())
                }
            })
        }),
    );
    insert_some(&mut map, "proxy", opts.proxy.clone().map(Value::String));
    insert_some(
        &mut map,
        "timeoutMs",
        opts.timeout_ms.map(|value| json!(value)),
    );
    insert_some(
        &mut map,
        "tempDir",
        opts.temp_dir.clone().map(Value::String),
    );
    Value::Object(map)
}

/// The details payload `execute` returns (index.ts:749-765): `batchProgress`
/// is the final snapshot (`completed === total`), `batchResult` the full
/// result — both structures ported shape-for-shape for the P2 TUI renderer.
pub fn batch_details_json(result: &BatchFetchResult, verbose: bool) -> serde_json::Value {
    let progress_items: Vec<serde_json::Value> = result
        .items
        .iter()
        .map(|item| {
            let mut map = serde_json::Map::new();
            map.insert("index".to_string(), json!(item.index));
            map.insert(
                "url".to_string(),
                json!(item
                    .request
                    .as_ref()
                    .map(|opts| opts.url.clone())
                    .unwrap_or_else(|| item.request_url.clone())),
            );
            map.insert("status".to_string(), json!(item.status));
            map.insert("progress".to_string(), json!(PROGRESS_DONE));
            insert_some(
                &mut map,
                "error",
                item.error.clone().map(|error| json!(error)),
            );
            serde_json::Value::Object(map)
        })
        .collect();
    let final_progress = json!({
        "items": progress_items,
        "total": result.total,
        "completed": result.total,
        "succeeded": result.succeeded,
        "failed": result.failed,
        "batchConcurrency": result.batch_concurrency,
    });

    let result_items: Vec<serde_json::Value> = result
        .items
        .iter()
        .map(|item| {
            let mut map = serde_json::Map::new();
            map.insert("index".to_string(), json!(item.index));
            insert_some(
                &mut map,
                "request",
                item.request.as_ref().map(fetch_options_json),
            );
            map.insert("status".to_string(), json!(item.status));
            map.insert("progress".to_string(), json!(PROGRESS_DONE));
            insert_some(
                &mut map,
                "error",
                item.error.clone().map(|error| json!(error)),
            );
            insert_some(
                &mut map,
                "result",
                item.result
                    .as_ref()
                    .map(|fetch| serde_json::to_value(fetch).unwrap_or(serde_json::Value::Null)),
            );
            serde_json::Value::Object(map)
        })
        .collect();
    let batch_result = json!({
        "items": result_items,
        "total": result.total,
        "succeeded": result.succeeded,
        "failed": result.failed,
        "batchConcurrency": result.batch_concurrency,
    });

    json!({
        "verbose": verbose,
        "started": true,
        "batchProgress": final_progress,
        "batchResult": batch_result,
    })
}

/// Parse each entry into `WebFetchParams`; entries that fail deserialization
/// degrade to per-item errors (see [`batch_item_from_parse_failure`]) — a
/// non-string `url` renders the JS template value the pipeline would see
/// upstream (`Invalid URL: undefined` / `Invalid URL: 123`).
fn parse_batch_requests(
    entries: &[serde_json::Value],
) -> Vec<Result<WebFetchParams, (String, String)>> {
    entries
        .iter()
        .map(
            |entry| match serde_json::from_value::<WebFetchParams>(entry.clone()) {
                Ok(params) => Ok(params),
                Err(error) => {
                    let url_hint =
                        js_template_render(entry.get("url").unwrap_or(&serde_json::Value::Null));
                    Err((url_hint, error.to_string()))
                }
            },
        )
        .collect()
}

/// JS template-literal rendering of a JSON value (`${value}`): the shapes a
/// malformed `url` can take upstream.
fn js_template_render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "undefined".to_string(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        other => format!("{other}"),
    }
}

/// Drive a parsed batch through the pipeline (worker pool above), folding
/// parse failures in as error items at their input positions. The pool only
/// carries the well-formed requests — the failures never touch the wire.
/// FR-P2-C: the shared progress table spans the FULL input (created from the
/// raw entries before parsing, like upstream's `createInitialProgressItems`);
/// parse-failure positions flip to `error` before the pool starts (the
/// TE-D26 degradation — upstream rejects the whole call in the host layer).
pub async fn execute_batch_entries(
    pipeline: &FetchPipeline,
    entries: &[serde_json::Value],
    defaults: &FetchToolDefaults,
    on_progress: Option<BatchProgressSink>,
) -> BatchFetchResult {
    let progress = BatchProgressState::new(
        create_initial_progress_items(entries),
        defaults.batch_concurrency,
        on_progress,
    );
    progress.emit_initial();

    let parsed = parse_batch_requests(entries);
    let mut valid_positions = Vec::new();
    let mut valid_requests = Vec::new();
    for (index, entry) in parsed.iter().enumerate() {
        match entry {
            Ok(params) => {
                valid_positions.push(index);
                valid_requests.push(params.clone());
            }
            Err((_, message)) => {
                progress.update(index, "error", Some(message.clone()), None);
            }
        }
    }
    let mut pool_items = execute_batch_fetch_with_progress(
        pipeline,
        &valid_requests,
        defaults,
        Some(progress),
        &valid_positions,
    )
    .await
    .items
    .into_iter();

    let mut items = Vec::with_capacity(parsed.len());
    for (index, entry) in parsed.iter().enumerate() {
        match entry {
            Ok(_) => {
                let mut item = pool_items
                    .next()
                    .expect("pool result count matches valid requests");
                item.index = index;
                items.push(item);
            }
            Err((url_hint, message)) => {
                items.push(batch_item_from_parse_failure(index, url_hint, message))
            }
        }
    }

    let succeeded = items.iter().filter(|item| item.status == "done").count() as u64;
    let failed = items.iter().filter(|item| item.status == "error").count() as u64;
    BatchFetchResult {
        total: parsed.len() as u64,
        succeeded,
        failed,
        items,
        batch_concurrency: defaults.batch_concurrency,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_concurrency_bounds() {
        assert_eq!(resolve_batch_concurrency(None), 8);
        assert_eq!(resolve_batch_concurrency(Some(0.0)), 8);
        assert_eq!(resolve_batch_concurrency(Some(f64::NAN)), 8);
        assert_eq!(resolve_batch_concurrency(Some(3.7)), 3);
        assert_eq!(resolve_batch_concurrency(Some(0.5)), 1);
    }

    #[test]
    fn defaults_resolution() {
        let defaults = resolve_fetch_tool_defaults(&FetchToolConfig::default());
        assert_eq!(defaults.max_chars, 50_000);
        assert_eq!(defaults.timeout_ms, 15_000);
        assert_eq!(defaults.browser, "chrome_145");
        assert_eq!(defaults.os, "windows");
        assert_eq!(defaults.batch_concurrency, 8);

        let overrides = resolve_fetch_tool_defaults(&FetchToolConfig {
            max_chars: Some(1000),
            browser: Some("firefox_147".to_string()),
            batch_concurrency: Some(2.0),
            ..FetchToolConfig::default()
        });
        assert_eq!(overrides.max_chars, 1000);
        assert_eq!(overrides.browser, "firefox_147");
        assert_eq!(overrides.batch_concurrency, 2);
    }

    #[test]
    fn batch_response_text_shape() {
        let ok_result = FetchResult::content(
            "https://example.com/a",
            "https://example.com/a",
            12,
            "body text",
            "chrome_145",
            "windows",
        );
        let batch = BatchFetchResult {
            items: vec![
                BatchFetchItemResult {
                    index: 0,
                    request_url: "https://example.com/a".to_string(),
                    status: "done",
                    error: None,
                    result: Some(ok_result),
                    request: None,
                },
                BatchFetchItemResult {
                    index: 1,
                    request_url: "https://example.com/bad".to_string(),
                    status: "error",
                    error: Some("Error: Invalid URL: bad".to_string()),
                    result: None,
                    request: None,
                },
            ],
            total: 2,
            succeeded: 1,
            failed: 1,
            batch_concurrency: 8,
        };
        let text = build_batch_fetch_response_text(&batch, true);
        assert!(text.contains("> Requests: 2"));
        assert!(text.contains("> Succeeded: 1"));
        assert!(text.contains("> Concurrency: 8"));
        assert!(text.contains("## [1/2] https://example.com/a"));
        assert!(text.contains("## [2/2] https://example.com/bad"));
        assert!(text.contains("> Error: Invalid URL: bad"));
    }
}
