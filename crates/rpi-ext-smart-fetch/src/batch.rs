//! Batch fetch pure surface: response-text rendering and tool-default
//! resolution — ports of upstream `tool.ts` (defaults/worker-pool types) and
//! the batch half of `format.ts` @ b0111612.
//!
//! TE06 ships the pure functions + fixtures (requirements §5.1); the
//! bounded-concurrency worker pool itself (FR-P1-1, `executeBatchFetchToolCall`
//! tool.ts:218-348) lands in TE07 with the `batch_web_fetch` registration.

use crate::constants::{
    DEFAULT_BATCH_CONCURRENCY, DEFAULT_BROWSER, DEFAULT_MAX_CHARS, DEFAULT_OS, DEFAULT_TIMEOUT_MS,
};
use crate::format::{build_fetch_response_text, build_header, HeaderValue};
use crate::types::{
    FetchOptions, FetchOutcome, FetchResult, FetchToolConfig, FetchToolDefaults, IncludeReplies,
    OutputFormat,
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
/// text; success items carry the result.
#[derive(Debug, Clone)]
pub struct BatchFetchItemResult {
    pub index: usize,
    pub request_url: String,
    pub status: &'static str, // "done" | "error"
    pub error: Option<String>,
    pub result: Option<FetchResult>,
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

/// Convenience for TE07: fold one pipeline outcome into a batch item.
#[allow(dead_code)]
pub fn batch_item_from_outcome(
    index: usize,
    request_url: &str,
    outcome: FetchOutcome,
    error_text_of: impl Fn(&crate::types::FetchError) -> String,
) -> BatchFetchItemResult {
    match outcome {
        FetchOutcome::Result(result) => BatchFetchItemResult {
            index,
            request_url: request_url.to_string(),
            status: "done",
            error: None,
            result: Some(result),
        },
        FetchOutcome::Error(error) => BatchFetchItemResult {
            index,
            request_url: request_url.to_string(),
            status: "error",
            error: Some(error_text_of(&error)),
            result: None,
        },
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
                },
                BatchFetchItemResult {
                    index: 1,
                    request_url: "https://example.com/bad".to_string(),
                    status: "error",
                    error: Some("Error: Invalid URL: bad".to_string()),
                    result: None,
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
