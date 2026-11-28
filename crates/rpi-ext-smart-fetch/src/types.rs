//! Serde types mirroring upstream `packages/core/src/types.ts` @ feb6da00
//! (smart-fetch-core v0.3.23). Field names serialize camelCase to keep the
//! details/JSON shape byte-compatible with the upstream tool results.

use serde::{Deserialize, Serialize};

/// `OutputFormat` (types.ts:1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Markdown,
    Html,
    Text,
    Json,
    Raw,
}

impl OutputFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutputFormat::Markdown => "markdown",
            OutputFormat::Html => "html",
            OutputFormat::Text => "text",
            OutputFormat::Json => "json",
            OutputFormat::Raw => "raw",
        }
    }
}

impl std::str::FromStr for OutputFormat {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Tool params arrive as JSON: serde parses the five literals directly;
        // this FromStr only serves internal call sites.
        match value {
            "markdown" => Ok(OutputFormat::Markdown),
            "html" => Ok(OutputFormat::Html),
            "text" => Ok(OutputFormat::Text),
            "json" => Ok(OutputFormat::Json),
            "raw" => Ok(OutputFormat::Raw),
            _ => Err(()),
        }
    }
}

/// `FingerprintOs` legal values (types.ts:2; wreq-js `EmulationOS`).
pub const FINGERPRINT_OS_VALUES: [&str; 5] = ["windows", "macos", "linux", "android", "ios"];

/// `IncludeRepliesOption` (types.ts:3): boolean or `"extractors"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncludeReplies {
    Extractors,
    All,
    None,
}

/// `FetchOptions` (types.ts:13-25) — the internal request shape the pipeline
/// consumes (built from tool params + resolved defaults). `temp_dir` rides
/// through untouched until the TE07 FR-P1-4 download branch consumes it.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub url: String,
    pub browser: Option<String>,
    pub os: Option<String>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    pub format: Option<OutputFormat>,
    pub max_chars: Option<u64>,
    pub remove_images: Option<bool>,
    pub include_replies: Option<IncludeReplies>,
    pub proxy: Option<String>,
    pub timeout_ms: Option<u64>,
    /// FR-P1-4 download branch consumes this (settings-resolved temp dir).
    pub temp_dir: Option<String>,
}

impl FetchOptions {
    /// `fetchWithClientRedirects` recursion re-enters with a replaced `url`,
    /// everything else preserved (meta refresh / alternate follow-up,
    /// extract.ts:1306-1311 / 1418-1423 / 1537-1542).
    pub fn with_url(&self, url: String) -> Self {
        let mut next = self.clone();
        next.url = url;
        next
    }
}

/// `FetchResult` (types.ts:27-54): one flat struct over both `kind` variants —
/// upstream serializes `content: ""` on file results, so a single shape with
/// optional file fields matches the JSON exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResult {
    pub kind: String,
    pub url: String,
    #[serde(rename = "finalUrl")]
    pub final_url: String,
    pub title: String,
    pub author: String,
    pub published: String,
    pub site: String,
    pub language: String,
    #[serde(rename = "wordCount")]
    pub word_count: u64,
    pub content: String,
    pub browser: String,
    pub os: String,
    #[serde(
        rename = "contentType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_type: Option<String>,
    #[serde(rename = "filePath", default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(rename = "fileSize", default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl FetchResult {
    /// Text content result factory (upstream inline object literals).
    #[allow(clippy::too_many_arguments)]
    pub fn content(
        url: impl Into<String>,
        final_url: impl Into<String>,
        word_count: u64,
        content: impl Into<String>,
        browser: impl Into<String>,
        os: impl Into<String>,
    ) -> Self {
        FetchResult {
            kind: "content".to_string(),
            url: url.into(),
            final_url: final_url.into(),
            title: String::new(),
            author: String::new(),
            published: String::new(),
            site: String::new(),
            language: String::new(),
            word_count,
            content: content.into(),
            browser: browser.into(),
            os: os.into(),
            content_type: None,
            file_path: None,
            file_size: None,
            mime_type: None,
        }
    }

    /// `isFileFetchResult` (format.ts:10-14).
    pub fn is_file(&self) -> bool {
        self.kind == "file"
    }
}

/// `FetchErrorCode` (types.ts:56-66).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchErrorCode {
    InvalidUrl,
    UnsupportedProtocol,
    HttpError,
    UnexpectedResponse,
    Timeout,
    NetworkError,
    ProcessingError,
    DownloadError,
    NoContent,
    TooManyRedirects,
}

impl FetchErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            FetchErrorCode::InvalidUrl => "invalid_url",
            FetchErrorCode::UnsupportedProtocol => "unsupported_protocol",
            FetchErrorCode::HttpError => "http_error",
            FetchErrorCode::UnexpectedResponse => "unexpected_response",
            FetchErrorCode::Timeout => "timeout",
            FetchErrorCode::NetworkError => "network_error",
            FetchErrorCode::ProcessingError => "processing_error",
            FetchErrorCode::DownloadError => "download_error",
            FetchErrorCode::NoContent => "no_content",
            FetchErrorCode::TooManyRedirects => "too_many_redirects",
        }
    }
}

impl Serialize for FetchErrorCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FetchErrorCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "invalid_url" => Ok(FetchErrorCode::InvalidUrl),
            "unsupported_protocol" => Ok(FetchErrorCode::UnsupportedProtocol),
            "http_error" => Ok(FetchErrorCode::HttpError),
            "unexpected_response" => Ok(FetchErrorCode::UnexpectedResponse),
            "timeout" => Ok(FetchErrorCode::Timeout),
            "network_error" => Ok(FetchErrorCode::NetworkError),
            "processing_error" => Ok(FetchErrorCode::ProcessingError),
            "download_error" => Ok(FetchErrorCode::DownloadError),
            "no_content" => Ok(FetchErrorCode::NoContent),
            "too_many_redirects" => Ok(FetchErrorCode::TooManyRedirects),
            _ => Err(serde::de::Error::custom("unknown FetchErrorCode")),
        }
    }
}

/// `FetchErrorPhase` (types.ts:68-75).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FetchErrorPhase {
    Validation,
    Connecting,
    Waiting,
    Loading,
    Processing,
    #[default]
    Unknown,
}

impl FetchErrorPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            FetchErrorPhase::Validation => "validation",
            FetchErrorPhase::Connecting => "connecting",
            FetchErrorPhase::Waiting => "waiting",
            FetchErrorPhase::Loading => "loading",
            FetchErrorPhase::Processing => "processing",
            FetchErrorPhase::Unknown => "unknown",
        }
    }
}

impl Serialize for FetchErrorPhase {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FetchErrorPhase {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "validation" => Ok(FetchErrorPhase::Validation),
            "connecting" => Ok(FetchErrorPhase::Connecting),
            "waiting" => Ok(FetchErrorPhase::Waiting),
            "loading" => Ok(FetchErrorPhase::Loading),
            "processing" => Ok(FetchErrorPhase::Processing),
            _ => Ok(FetchErrorPhase::Unknown),
        }
    }
}

/// `FetchError` (types.ts:76-89). Optional context fields serialize only when
/// set — upstream JS omits `undefined` keys in JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<FetchErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<FetchErrorPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(rename = "timeoutMs", default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "finalUrl", default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(
        rename = "statusCode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub status_code: Option<u16>,
    #[serde(
        rename = "statusText",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub status_text: Option<String>,
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(
        rename = "contentLength",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_length: Option<u64>,
    #[serde(
        rename = "downloadedBytes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub downloaded_bytes: Option<u64>,
}

impl FetchError {
    /// Minimal builder for the common `{error, code, phase, retryable, url}`
    /// validation shape (extract.ts:1152-1158, 1162-1168).
    pub fn validation(message: String, code: FetchErrorCode, url: &str) -> Self {
        FetchError {
            error: message,
            code: Some(code),
            phase: Some(FetchErrorPhase::Validation),
            retryable: Some(false),
            timeout_ms: None,
            url: Some(url.to_string()),
            final_url: None,
            status_code: None,
            status_text: None,
            mime_type: None,
            content_length: None,
            downloaded_bytes: None,
        }
    }
}

/// Pipeline outcome: upstream returns `FetchResult | FetchError` discriminated
/// by the `error` key (`isError`, extract.ts:1715-1719).
#[derive(Debug, Clone)]
pub enum FetchOutcome {
    Result(FetchResult),
    Error(FetchError),
}

impl From<FetchError> for FetchOutcome {
    fn from(error: FetchError) -> Self {
        FetchOutcome::Error(error)
    }
}

impl From<FetchResult> for FetchOutcome {
    fn from(result: FetchResult) -> Self {
        FetchOutcome::Result(result)
    }
}

/// `ExtractedContent` (types.ts:126-135) — the extraction trait output.
/// `extractor_type` rides for diagnostics (upstream defuddle's extractor
/// name; not surfaced in results yet).
#[derive(Debug, Clone, Default)]
pub struct ExtractedContent {
    pub content: Option<String>,
    pub word_count: u64,
    pub title: Option<String>,
    pub author: Option<String>,
    pub published: Option<String>,
    pub site: Option<String>,
    pub language: Option<String>,
    #[allow(dead_code)]
    pub extractor_type: Option<String>,
}

/// `FetchToolConfig` / `FetchToolDefaults` (types.ts:185-205): resolved
/// settings → runtime defaults (tool.ts:37-50).
#[derive(Debug, Clone, Default)]
pub struct FetchToolConfig {
    pub max_chars: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub browser: Option<String>,
    pub os: Option<String>,
    pub remove_images: Option<bool>,
    pub include_replies: Option<IncludeReplies>,
    pub batch_concurrency: Option<f64>,
    pub temp_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FetchToolDefaults {
    pub max_chars: u64,
    pub timeout_ms: u64,
    pub browser: String,
    pub os: String,
    pub remove_images: bool,
    pub include_replies: IncludeReplies,
    pub batch_concurrency: u64,
    pub temp_dir: Option<String>,
}

/// Tool parameter payload for `web_fetch` (FR-P0-1; serde replaces TypeBox).
/// Upstream TypeBox `additionalProperties` is unset for the base tool
/// (tool.ts:52-116), so unknown keys are ignored rather than rejected.
#[derive(Debug, Clone, Deserialize)]
pub struct WebFetchParams {
    pub url: String,
    pub browser: Option<String>,
    pub os: Option<String>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    pub max_chars: Option<u64>,
    #[serde(rename = "timeoutMs")]
    pub timeout_ms: Option<u64>,
    pub format: Option<OutputFormatSerde>,
    pub remove_images: Option<bool>,
    #[serde(default, deserialize_with = "parse_include_replies")]
    pub include_replies: Option<IncludeReplies>,
    pub proxy: Option<String>,
    pub verbose: Option<bool>,
}

/// `includeReplies: boolean | "extractors"` (tool.ts:103-108). A unit variant
/// cannot capture the string literal under `#[serde(untagged)]`, so the union
/// is parsed by hand.
fn parse_include_replies<'de, D>(deserializer: D) -> Result<Option<IncludeReplies>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(true)) => Ok(Some(IncludeReplies::All)),
        Some(serde_json::Value::Bool(false)) => Ok(Some(IncludeReplies::None)),
        Some(serde_json::Value::String(s)) if s == "extractors" => {
            Ok(Some(IncludeReplies::Extractors))
        }
        Some(_) => Err(serde::de::Error::custom(
            "includeReplies must be a boolean or \"extractors\"",
        )),
    }
}

/// Serde proxy: `format` is the five-string literal union (tool.ts:84-97).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormatSerde {
    Markdown,
    Html,
    Text,
    Json,
    Raw,
}

impl From<OutputFormatSerde> for OutputFormat {
    fn from(value: OutputFormatSerde) -> Self {
        match value {
            OutputFormatSerde::Markdown => OutputFormat::Markdown,
            OutputFormatSerde::Html => OutputFormat::Html,
            OutputFormatSerde::Text => OutputFormat::Text,
            OutputFormatSerde::Json => OutputFormat::Json,
            OutputFormatSerde::Raw => OutputFormat::Raw,
        }
    }
}

/// Marker appended by `truncate_content` when content exceeds maxChars
/// (upstream index.ts:159 `TRUNCATION_MARKER`).
pub const TRUNCATION_MARKER: &str = "\n\n[... truncated]";

/// `WebFetchContentSummary` / `WebFetchFileSummary` (index.ts:105-134 @
/// feb6da00): compact, persistable summary of a successful `web_fetch`
/// result. The extracted page text travels in the tool `content` payload, so
/// `details` keeps only metadata and never duplicates the body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WebFetchResultSummary {
    Content {
        url: String,
        final_url: String,
        ok: bool,
        char_count: u64,
        word_count: u64,
        truncated: bool,
        title: String,
        published: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
    File {
        url: String,
        final_url: String,
        ok: bool,
        file_path: String,
        file_size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
}

/// `wasTruncatedAtMaxChars` (index.ts:163-165): the trailing marker is the
/// only evidence a legacy result carries. `chars().count()` keeps the
/// registered UTF-16-code-unit → Rust-char [VARIANT].
pub fn was_truncated_at_max_chars(content: &str, max_chars: u64) -> bool {
    content.chars().count() as u64 > max_chars && content.ends_with(TRUNCATION_MARKER)
}

impl WebFetchResultSummary {
    /// `buildFetchResultSummary` (index.ts:167-205).
    pub fn from_result(result: &FetchResult, max_chars: u64) -> Self {
        if result.is_file() {
            return WebFetchResultSummary::File {
                url: result.url.clone(),
                final_url: result.final_url.clone(),
                ok: true,
                file_path: result.file_path.clone().unwrap_or_default(),
                file_size: result.file_size.unwrap_or(0),
                mime_type: result.mime_type.clone().filter(|mime| !mime.is_empty()),
            };
        }
        WebFetchResultSummary::Content {
            url: result.url.clone(),
            final_url: result.final_url.clone(),
            ok: true,
            char_count: result.content.chars().count() as u64,
            word_count: result.word_count,
            truncated: was_truncated_at_max_chars(&result.content, max_chars),
            title: result.title.clone(),
            published: result.published.clone(),
            content_type: result.content_type.clone().filter(|mime| !mime.is_empty()),
        }
    }

    /// Legacy read path (`resolveWebFetchSummary`, index.ts:207-238): a
    /// session persisted before the compact summary stored the full result;
    /// derive the summary in memory and never write the body back. The
    /// legacy `details.maxChars` may be the settings default rather than the
    /// call's limit, so the trailing marker is also consulted (best-effort;
    /// page text that literally ends with the marker is reported truncated,
    /// matching upstream).
    pub fn from_legacy_result(result: &FetchResult, max_chars: u64) -> Self {
        let mut summary = Self::from_result(result, max_chars);
        if let WebFetchResultSummary::Content {
            truncated,
            char_count,
            ..
        } = &mut summary
        {
            *truncated |= result.content.ends_with(TRUNCATION_MARKER);
            // Keep the field order/typing obvious for the JSON golden: the
            // char count is recomputed from the same body.
            *char_count = result.content.chars().count() as u64;
        }
        summary
    }

    pub fn url(&self) -> &str {
        match self {
            WebFetchResultSummary::Content { url, .. } => url,
            WebFetchResultSummary::File { url, .. } => url,
        }
    }

    pub fn final_url(&self) -> &str {
        match self {
            WebFetchResultSummary::Content { final_url, .. } => final_url,
            WebFetchResultSummary::File { final_url, .. } => final_url,
        }
    }
}

/// `BatchFetchItemSummary` (index.ts:136-151): per-item metadata only; the
/// item's extracted body is not persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchFetchItemSummary {
    pub index: u64,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    pub ok: bool,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub word_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// `BatchFetchResultSummary` (index.ts:153-160).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchFetchResultSummary {
    pub total: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub batch_concurrency: u64,
    pub duration_ms: u64,
    pub items: Vec<BatchFetchItemSummary>,
}

/// `resolveWebFetchSummary` (index.ts:207-238): prefer the compact summary;
/// otherwise derive one from a legacy `fetchResult`. Malformed payloads
/// degrade to `None` instead of failing the render.
pub fn resolve_web_fetch_summary(
    details: &serde_json::Value,
    default_max_chars: u64,
) -> Option<WebFetchResultSummary> {
    if let Some(summary) = details.get("fetchSummary")
        && let Ok(summary) = serde_json::from_value::<WebFetchResultSummary>(summary.clone())
    {
        return Some(summary);
    }
    let legacy = details.get("fetchResult")?;
    let result = serde_json::from_value::<FetchResult>(legacy.clone()).ok()?;
    let max_chars = details
        .get("maxChars")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(default_max_chars);
    Some(WebFetchResultSummary::from_legacy_result(
        &result, max_chars,
    ))
}

#[cfg(test)]
mod summary_tests {
    use super::*;
    use serde_json::{Value, json};

    fn content_result() -> FetchResult {
        let mut result = FetchResult::content(
            "https://ex.com/a",
            "https://ex.com/final",
            12,
            "hello world body",
            "chrome_145",
            "windows",
        );
        result.title = "T".to_string();
        result.published = "2026-01-01".to_string();
        result.content_type = Some("text/html".to_string());
        result
    }

    fn keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn content_summary_golden_camel_case_keys() {
        let summary = WebFetchResultSummary::from_result(&content_result(), 50_000);
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            keys(&value),
            vec![
                "charCount",
                "contentType",
                "finalUrl",
                "kind",
                "ok",
                "published",
                "title",
                "truncated",
                "url",
                "wordCount",
            ]
        );
        assert_eq!(value["kind"], json!("content"));
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["charCount"], json!(16));
        assert_eq!(value["truncated"], json!(false));
        let text = value.to_string();
        for snake in [
            "final_url",
            "char_count",
            "word_count",
            "content_type",
            "file_path",
            "file_size",
            "mime_type",
        ] {
            assert!(!text.contains(snake), "snake_case leaked: {snake}");
        }
    }

    #[test]
    fn file_summary_golden_camel_case_keys() {
        let mut result = FetchResult::content(
            "https://ex.com/report",
            "https://ex.com/report.pdf",
            0,
            "",
            "chrome_145",
            "windows",
        );
        result.kind = "file".to_string();
        result.file_path = Some("/tmp/report.pdf".to_string());
        result.file_size = Some(123);
        result.mime_type = Some("application/pdf".to_string());
        let summary = WebFetchResultSummary::from_result(&result, 50_000);
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            keys(&value),
            vec![
                "filePath", "fileSize", "finalUrl", "kind", "mimeType", "ok", "url"
            ]
        );
        assert_eq!(value["kind"], json!("file"));
        assert_eq!(value["fileSize"], json!(123));
    }

    #[test]
    fn optional_fields_are_omitted_when_absent() {
        let summary = WebFetchResultSummary::from_result(&content_result(), 50_000);
        let mut value = serde_json::to_value(&summary).unwrap();
        value.as_object_mut().unwrap().remove("contentType");
        let value: WebFetchResultSummary = serde_json::from_value(value).unwrap();
        let rendered = serde_json::to_value(&value).unwrap();
        assert!(rendered.get("contentType").is_none());
    }

    #[test]
    fn truncation_requires_length_and_marker() {
        let long = format!("{}{}", "a".repeat(11), TRUNCATION_MARKER);
        assert!(was_truncated_at_max_chars(&long, 10));
        assert!(!was_truncated_at_max_chars(&long, 100));
        assert!(!was_truncated_at_max_chars("short body", 10));
    }

    #[test]
    fn resolve_prefers_summary_then_legacy() {
        let result = content_result();
        let summary = WebFetchResultSummary::from_result(&result, 50_000);
        let details = json!({
            "fetchSummary": serde_json::to_value(&summary).unwrap(),
            "fetchResult": serde_json::to_value(&result).unwrap(),
        });
        let resolved = resolve_web_fetch_summary(&details, 50_000).unwrap();
        assert!(matches!(resolved, WebFetchResultSummary::Content { .. }));

        let legacy_only = json!({
            "maxChars": 50_000,
            "fetchResult": serde_json::to_value(&result).unwrap(),
        });
        assert!(resolve_web_fetch_summary(&legacy_only, 50_000).is_some());
        assert!(resolve_web_fetch_summary(&json!({}), 50_000).is_none());
        assert!(
            resolve_web_fetch_summary(&json!({ "fetchResult": { "kind": "content" } }), 50_000)
                .is_none(),
            "malformed legacy result degrades"
        );
    }

    #[test]
    fn legacy_marker_marks_truncated_even_with_default_max_chars() {
        let mut result = content_result();
        result.content = format!("{}{}", "a".repeat(20), TRUNCATION_MARKER);
        let details = json!({
            // Legacy sessions persisted the settings default, not the call's
            // limit: the marker still reports truncation (index.ts:228-236).
            "maxChars": 1000,
            "fetchResult": serde_json::to_value(&result).unwrap(),
        });
        let Some(WebFetchResultSummary::Content { truncated, .. }) =
            resolve_web_fetch_summary(&details, 1000)
        else {
            panic!("content summary");
        };
        assert!(truncated);
    }
}
