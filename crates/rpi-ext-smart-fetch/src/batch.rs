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
    build_fetch_error_response_text, build_fetch_response_text, build_header, HeaderValue,
};
use crate::pipeline::FetchPipeline;
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

/// `executeBatchFetchToolCall` (tool.ts:218-348): a worker pool of
/// `min(concurrency, len)` claim-next workers — bounded in-flight requests,
/// per-item error isolation, results kept in input order (each worker writes
/// by index). Progress snapshots ride only in the final result (the
/// `on_update` ABI gap, design §1.3 #3).
pub async fn execute_batch_fetch(
    pipeline: &FetchPipeline,
    requests: &[WebFetchParams],
    defaults: &FetchToolDefaults,
) -> BatchFetchResult {
    let worker_count = if requests.is_empty() {
        0
    } else {
        std::cmp::min(defaults.batch_concurrency as usize, requests.len())
    };
    let results: Arc<Mutex<Vec<Option<BatchFetchItemResult>>>> =
        Arc::new(Mutex::new((0..requests.len()).map(|_| None).collect()));
    let next_index = Arc::new(AtomicUsize::new(0));

    // Upstream's `nextIndex` claim loop (tool.ts:269-333), one async worker
    // per slot polled concurrently — the single-threaded event-loop
    // concurrency profile of `Promise.all(Array.from(..., worker))`.
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let results = Arc::clone(&results);
        let next_index = Arc::clone(&next_index);
        // Borrows pipeline/requests/defaults outlive the join below.
        workers.push(async move {
            loop {
                let index = next_index.fetch_add(1, Ordering::SeqCst);
                if index >= requests.len() {
                    return;
                }
                let request = &requests[index];
                let opts = build_fetch_options_from_params(request, defaults);
                let outcome = pipeline.fetch(&opts).await;
                let item = batch_item_from_outcome(index, &opts, outcome);
                // recover a poisoned lock (the critical section cannot panic;
                // every request must land its item or the count invariant
                // downstream breaks)
                let mut slot = results.lock().unwrap_or_else(|error| error.into_inner());
                slot[index] = Some(item);
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
pub async fn execute_batch_entries(
    pipeline: &FetchPipeline,
    entries: &[serde_json::Value],
    defaults: &FetchToolDefaults,
) -> BatchFetchResult {
    let parsed = parse_batch_requests(entries);
    let mut valid_positions = Vec::new();
    let mut valid_requests = Vec::new();
    for (index, entry) in parsed.iter().enumerate() {
        if let Ok(params) = entry {
            valid_positions.push(index);
            valid_requests.push(params.clone());
        }
    }
    let mut pool_items = execute_batch_fetch(pipeline, &valid_requests, defaults)
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
