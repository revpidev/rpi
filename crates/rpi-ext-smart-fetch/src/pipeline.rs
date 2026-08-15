//! Core fetch pipeline — 1:1 port of upstream
//! `packages/core/src/extract.ts:fetchWithClientRedirects` @ b0111612
//! (FR-P0-2/5/6/8/9/10/11 + FR-P1-2/3/4 recursion, download and fallback
//! branches).
//! Phase tracking marks by pipeline position (design §3.1): send()-stage
//! timeouts report `connecting` (upstream's event stream would split that
//! into connecting/waiting — declared deviation, task file).

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use scraper::{Html, Selector};

use crate::constants::*;
use crate::extract::{DomSmoothieExtractor, ExtractOptions, Extractor};
use crate::format::{
    estimate_word_count, format_byte_count, markdown_to_text, parse_and_format_json,
    render_json_content, truncate_content,
};
use crate::http::{self, FetchFailure, HttpRequest};
use crate::types::{
    FetchError, FetchErrorPhase, FetchOutcome, FetchResult, IncludeReplies, OutputFormat,
};

/// Patch table for `mime_guess` ↔ `mime-types` (npm) drift in
/// download-extension mapping (design §3.4). The parity fixtures arbitrate
/// which entries are needed — `application/octet-stream` is the first
/// recorded divergence (mime-types maps `.bin`, mime_guess picks `.aaf`).
pub static MIME_PATCH_TABLE: LazyLock<HashMap<&'static str, String>> =
    LazyLock::new(|| HashMap::from([("application/octet-stream", ".bin".to_string())]));

// ===== content-type & response classification (extract.ts:75-98, 346-349, 904-920) =====

/// `normalizeContentType` (extract.ts:75-77).
pub fn normalize_content_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// `isAttachmentDisposition` (extract.ts:79-81).
pub fn is_attachment_disposition(content_disposition: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^attachment(?:\s*;|\s*$)").expect("valid regex"));
    RE.is_match(content_disposition.trim())
}

/// `isTextualContentType` (extract.ts:83-98).
pub fn is_textual_content_type(content_type: &str) -> bool {
    let normalized = normalize_content_type(content_type);
    normalized.starts_with("text/")
        || normalized == "application/json"
        || normalized == "text/json"
        || normalized.ends_with("+json")
        || normalized == "application/xml"
        || normalized == "text/xml"
        || normalized.ends_with("+xml")
        || normalized == "application/javascript"
        || normalized == "application/x-javascript"
        || normalized == "application/ecmascript"
        || normalized == "image/svg+xml"
}

/// `isPlainTextContentType` (extract.ts:346-349).
pub fn is_plain_text_content_type(content_type: &str) -> bool {
    let normalized = normalize_content_type(content_type);
    normalized == "text/plain" || normalized == "text/markdown"
}

/// `isJsonContentType` (extract.ts:904-911).
pub fn is_json_content_type(content_type: &str) -> bool {
    let normalized = normalize_content_type(content_type);
    normalized == "application/json" || normalized == "text/json" || normalized.ends_with("+json")
}

/// `isLikelyJsonBody` (extract.ts:913-916).
pub fn is_likely_json_body(body: &str) -> bool {
    let trimmed = body.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

/// `isJsonResponse` (extract.ts:918-920).
pub fn is_json_response(content_type: &str, body: &str) -> bool {
    is_json_content_type(content_type) || is_likely_json_body(body)
}

/// `resolveAcceptHeader` (extract.ts:898-902).
pub fn resolve_accept_header(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Json => DEFAULT_JSON_ACCEPT_HEADER,
        OutputFormat::Raw => DEFAULT_RAW_ACCEPT_HEADER,
        _ => DEFAULT_ACCEPT_HEADER,
    }
}

/// `decodeHtmlAttribute` (extract.ts:922-929): every entity matches
/// case-INSENSITIVELY (`/gi` flags upstream — `&AMP;` decodes just like
/// `&amp;`).
pub fn decode_html_attribute(value: &str) -> String {
    use std::sync::LazyLock;
    static AMP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&amp;").expect("valid"));
    static QUOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&quot;").expect("valid"));
    static APOS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)&(#39|apos);").expect("valid"));
    static LT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&lt;").expect("valid"));
    static GT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&gt;").expect("valid"));
    let value = AMP.replace_all(value, "&");
    let value = QUOT.replace_all(&value, "\"");
    let value = APOS.replace_all(&value, "'");
    let value = LT.replace_all(&value, "<");
    GT.replace_all(&value, ">").into_owned()
}

/// `parseContentLengthHeader` (extract.ts:654-658): parseInt semantics —
/// leading integer prefix, non-finite/negative → None.
pub fn parse_content_length_header(value: Option<&str>) -> Option<u64> {
    let value = value?;
    let parsed = value.trim();
    let digits: String = parsed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// `extractHostname` (extract.ts:686-692).
pub fn extract_hostname(url: &str) -> String {
    url::Url::parse(url)
        .map(|parsed| parsed.host_str().unwrap_or_default().to_string())
        .unwrap_or_else(|_| url.to_string())
}

// ===== meta refresh & alternate links (FR-P1-2/P1-3 pure halves) =====

/// `extractClientSideRedirect` (extract.ts:975-1010): scan the first 4096
/// chars of the raw body for a `<meta http-equiv=refresh>` and resolve the
/// target. `delay` must parse finite in [0, 30); the target resolves against
/// `base_url` and must differ from it.
///
/// Boundary note: upstream slices 4096 UTF-16 code units; this port slices
/// 4096 chars — same declared [VARIANT] class as FR-P0-9 truncation.
pub fn extract_client_side_redirect(body: &str, base_url: &str) -> Option<String> {
    let snippet: String = body.chars().take(META_REFRESH_SCAN_WINDOW).collect();
    static RE_META: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)<meta\b[^>]*http-equiv=["']?refresh["']?[^>]*content=["']?([^"'>]*)["']?[^>]*>"#,
        )
        .expect("valid regex")
    });
    let refresh_content = RE_META.captures(&snippet)?.get(1)?.as_str();

    let decoded = decode_html_attribute(refresh_content);
    let mut parts = decoded.split(';');
    let delay_part = parts.next().unwrap_or("");
    let rest = parts.collect::<Vec<_>>().join(";");
    let delay_seconds = js_parse_float(delay_part.trim())?;

    static RE_URL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\burl\s*=\s*(.+)$").expect("valid regex"));
    let raw_target = RE_URL
        .captures(&rest)?
        .get(1)?
        .as_str()
        .trim()
        .trim_start_matches(['\'', '"'])
        .trim_end_matches(['\'', '"'])
        .to_string();

    if raw_target.is_empty() || !(0.0..30.0).contains(&delay_seconds) {
        return None;
    }

    let base = url::Url::parse(base_url).ok()?;
    let target = url::Url::options()
        .base_url(Some(&base))
        .parse(&raw_target)
        .ok()?;
    let target = target.to_string();
    if target == base_url {
        None
    } else {
        Some(target)
    }
}

/// JS `parseFloat` subset (extract.ts:991): longest numeric prefix, None for
/// NaN. `f64::from_str` alone rejects "3x" which parseFloat accepts.
fn js_parse_float(value: &str) -> Option<f64> {
    if value.is_empty() {
        return None;
    }
    for end in (1..=value.len()).rev() {
        if !value.is_char_boundary(end) {
            continue;
        }
        if let Ok(parsed) = value[..end].parse::<f64>() {
            return parsed.is_finite().then_some(parsed);
        }
    }
    None
}

/// `extractQualifiedAlternateLinks` (extract.ts:931-973): head `<link
/// rel=alternate type=…>` whose type matches the requested format, resolved
/// absolute, deduped, self excluded.
pub fn extract_qualified_alternate_links(
    document: &Html,
    base_url: &str,
    format: OutputFormat,
) -> Vec<String> {
    let accepted: &[&str] = match format {
        OutputFormat::Markdown => &["text/markdown", "text/x-markdown"],
        OutputFormat::Text => &["text/plain", "text/markdown", "text/x-markdown"],
        OutputFormat::Html => &["text/html", "application/xhtml+xml"],
        OutputFormat::Json => &["application/json", "text/json"],
        OutputFormat::Raw => &[],
    };
    static HEAD_LINKS: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("head link").expect("valid selector"));
    static RE_WHITESPACE_SPLIT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));

    let base = match url::Url::parse(base_url) {
        Ok(base) => base,
        Err(_) => return Vec::new(),
    };

    let mut candidates: Vec<String> = Vec::new();
    for link in document.select(&HEAD_LINKS) {
        let rel = link.value().attr("rel").unwrap_or_default();
        if !RE_WHITESPACE_SPLIT
            .split(rel.to_lowercase().as_str())
            .any(|token| token == "alternate")
        {
            continue;
        }
        let link_type = normalize_content_type(link.value().attr("type").unwrap_or_default());
        let is_accepted = accepted.contains(&link_type.as_str())
            || (format == OutputFormat::Json && link_type.ends_with("+json"));
        if !is_accepted {
            continue;
        }
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Ok(target) = url::Url::options().base_url(Some(&base)).parse(href) else {
            continue;
        };
        let target = target.to_string();
        if target != base_url && !candidates.contains(&target) {
            candidates.push(target);
        }
    }
    candidates
}

// ===== error construction (extract.ts:694-859) =====

/// `FetchErrorContext` (extract.ts:614-624).
#[derive(Debug, Clone)]
pub struct FetchErrorContext {
    pub url: String,
    pub final_url: Option<String>,
    pub phase: FetchErrorPhase,
    pub timeout_ms: u64,
    pub status_code: Option<u16>,
    pub status_text: Option<String>,
    pub mime_type: Option<String>,
    pub content_length: Option<u64>,
    pub downloaded_bytes: Option<u64>,
}

impl FetchErrorContext {
    pub fn new(url: &str, timeout_ms: u64) -> Self {
        FetchErrorContext {
            url: url.to_string(),
            final_url: None,
            phase: FetchErrorPhase::Connecting,
            timeout_ms,
            status_code: None,
            status_text: None,
            mime_type: None,
            content_length: None,
            downloaded_bytes: None,
        }
    }
}

/// `buildTimeoutError` (extract.ts:694-773): per-phase message templates.
pub fn build_timeout_error(context: &FetchErrorContext) -> FetchError {
    let target_url = context
        .final_url
        .clone()
        .unwrap_or_else(|| context.url.clone());
    let timeout_label = format!("{}ms", context.timeout_ms);

    let describe = |error: String, phase, extra_status: bool| {
        let mut fetch_error = FetchError {
            error,
            code: Some(crate::types::FetchErrorCode::Timeout),
            phase: Some(phase),
            retryable: Some(true),
            timeout_ms: Some(context.timeout_ms),
            url: Some(context.url.clone()),
            final_url: context.final_url.clone(),
            status_code: None,
            status_text: None,
            mime_type: None,
            content_length: None,
            downloaded_bytes: None,
        };
        if extra_status {
            fetch_error.status_code = context.status_code;
            fetch_error.status_text = context.status_text.clone();
            fetch_error.mime_type = context.mime_type.clone();
            fetch_error.content_length = context.content_length;
            fetch_error.downloaded_bytes = context.downloaded_bytes;
        }
        fetch_error
    };

    match context.phase {
        FetchErrorPhase::Connecting => describe(
            format!("Timeout of {timeout_label} exceeded while connecting to {target_url}."),
            FetchErrorPhase::Connecting,
            false,
        ),
        FetchErrorPhase::Waiting => describe(
            format!(
                "Timeout of {timeout_label} exceeded while waiting for {target_url} to start responding."
            ),
            FetchErrorPhase::Waiting,
            false,
        ),
        FetchErrorPhase::Loading => {
            // JS truthiness: contentLength 0 falls to the plain branch.
            let size_hint = if context.content_length.is_some_and(|len| len > 0) {
                let len = context.content_length.unwrap_or(0);
                let kind = match &context.mime_type {
                    Some(mime) if !is_textual_content_type(mime) => "file",
                    _ => "response",
                };
                format!(" a {} {kind}", format_byte_count(len))
            } else {
                " the response body".to_string()
            };
            describe(
                format!(
                    "Timeout of {timeout_label} exceeded while downloading{size_hint} from {target_url}."
                ),
                FetchErrorPhase::Loading,
                true,
            )
        }
        FetchErrorPhase::Processing => describe(
            format!(
                "Timeout of {timeout_label} exceeded while processing the response from {target_url}."
            ),
            FetchErrorPhase::Processing,
            true,
        ),
        _ => describe(
            format!("Timeout of {timeout_label} exceeded while fetching {target_url}."),
            context.phase,
            true,
        ),
    }
}

/// `buildThrownFetchError` (extract.ts:775-859) for classified transport
/// failures; `message` feeds the template verbatim (wreq display text — the
/// engine-message divergence vs wreq-js napi text is accepted, templates are
/// what must match byte-for-byte).
pub fn build_thrown_fetch_error(
    failure: &http::TransportFailure,
    context: &FetchErrorContext,
) -> FetchError {
    use crate::types::FetchErrorCode;

    if matches!(failure, http::TransportFailure::Timeout) {
        return build_timeout_error(context);
    }

    let hostname = extract_hostname(&context.url);

    let fixed = |error: String, retryable: bool| FetchError {
        error,
        code: Some(FetchErrorCode::NetworkError),
        phase: Some(FetchErrorPhase::Connecting),
        retryable: Some(retryable),
        timeout_ms: None,
        url: Some(context.url.clone()),
        final_url: context.final_url.clone(),
        status_code: None,
        status_text: None,
        mime_type: None,
        content_length: None,
        downloaded_bytes: None,
    };

    match failure {
        http::TransportFailure::Dns => fixed(
            format!(
                "DNS error: failed to lookup address for {hostname}. Check the URL and try again."
            ),
            false,
        ),
        http::TransportFailure::Connect => fixed(
            format!(
                "Connection failed to {hostname}. The server may be unreachable or blocking requests."
            ),
            true,
        ),
        http::TransportFailure::Tls => fixed(
            format!(
                "TLS/SSL error connecting to {hostname}. The server's certificate may be invalid."
            ),
            false,
        ),
        http::TransportFailure::Timeout => unreachable!("handled above"),
        http::TransportFailure::Other { message } => {
            let effective_phase = context.phase;
            let target_url = context
                .final_url
                .clone()
                .unwrap_or_else(|| context.url.clone());
            let phase_description = match effective_phase {
                FetchErrorPhase::Loading => "downloading the response",
                FetchErrorPhase::Waiting => "waiting for the server response",
                FetchErrorPhase::Connecting => "connecting",
                _ => "fetching",
            };
            FetchError {
                error: if effective_phase == FetchErrorPhase::Processing {
                    format!("Failed while processing the response from {target_url}: {message}")
                } else {
                    format!("Request failed while {phase_description} for {target_url}: {message}")
                },
                code: if effective_phase == FetchErrorPhase::Processing {
                    Some(FetchErrorCode::ProcessingError)
                } else if effective_phase == FetchErrorPhase::Loading
                    && context.mime_type.is_some()
                {
                    Some(FetchErrorCode::DownloadError)
                } else {
                    Some(FetchErrorCode::NetworkError)
                },
                phase: Some(effective_phase),
                retryable: Some(effective_phase != FetchErrorPhase::Processing),
                timeout_ms: Some(context.timeout_ms),
                url: Some(context.url.clone()),
                final_url: context.final_url.clone(),
                status_code: context.status_code,
                status_text: context.status_text.clone(),
                mime_type: context.mime_type.clone(),
                content_length: context.content_length,
                downloaded_bytes: context.downloaded_bytes,
            }
        }
    }
}

/// Map an `http::FetchFailure` (raised at pipeline position `context.phase`)
/// into the FetchError model — the port of upstream's `catch →
/// buildThrownFetchError` for engine-thrown errors, including the
/// wreq-js RequestError path for invalid profiles (FR-P0-3).
/// `Other` messages re-classify through the upstream message regexes first
/// (mocked transports replay raw engine messages, exactly like upstream's
/// `buildThrownFetchError` does on `error.message`).
pub fn map_fetch_failure(failure: &FetchFailure, context: &FetchErrorContext) -> FetchError {
    match failure {
        FetchFailure::InvalidInput(message) => build_thrown_fetch_error(
            &http::classify_message(message).unwrap_or(http::TransportFailure::Other {
                message: message.clone(),
            }),
            context,
        ),
        FetchFailure::Transport { failure, final_url } => {
            let mut context = context.clone();
            if let Some(final_url) = final_url {
                context.final_url = Some(final_url.clone());
            }
            let failure = match failure {
                http::TransportFailure::Other { message } => http::classify_message(message)
                    .unwrap_or(http::TransportFailure::Other {
                        message: message.clone(),
                    }),
                classified => classified.clone(),
            };
            build_thrown_fetch_error(&failure, &context)
        }
    }
}

// ===== result builders (extract.ts:351-389, 1012-1046) =====

/// `renderPlainTextContent` (extract.ts:351-360).
pub fn render_plain_text_content(body: &str, format: OutputFormat) -> String {
    if format == OutputFormat::Html {
        return format!(
            "<pre>{}</pre>",
            body.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        );
    }
    body.to_string()
}

/// `buildPlainTextResult` (extract.ts:362-389).
#[allow(clippy::too_many_arguments)]
pub fn build_plain_text_result(
    opts_url: &str,
    final_url: &str,
    raw_body: &str,
    format: OutputFormat,
    max_chars: u64,
    browser: &str,
    os: &str,
) -> FetchResult {
    let normalized_body = raw_body.replace("\r\n", "\n");
    let normalized_body =
        normalized_body.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let mut result = FetchResult::content(
        opts_url,
        final_url,
        estimate_word_count(normalized_body),
        truncate_content(
            &render_plain_text_content(normalized_body, format),
            max_chars,
        ),
        browser,
        os,
    );
    result.site = url::Url::parse(final_url)
        .map(|parsed| parsed.host_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    result
}

/// `buildJsonResult` (extract.ts:1012-1046). Returns Err on invalid JSON
/// (upstream's `parseAndFormatJson` error shape).
// The Err side is the full FetchError context struct (upstream returns it
// as a plain object); boxing just for the lint would diverge from the port's
// data shape for no gain.
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
pub fn build_json_result(
    opts_url: &str,
    final_url: &str,
    raw_body: &str,
    format: OutputFormat,
    max_chars: u64,
    browser: &str,
    os: &str,
) -> Result<FetchResult, FetchError> {
    let formatted = parse_and_format_json(raw_body).map_err(|message| FetchError {
        error: message,
        code: None,
        phase: None,
        retryable: None,
        timeout_ms: None,
        url: None,
        final_url: None,
        status_code: None,
        status_text: None,
        mime_type: None,
        content_length: None,
        downloaded_bytes: None,
    })?;

    let mut result = FetchResult::content(
        opts_url,
        final_url,
        estimate_word_count(&formatted),
        truncate_content(&render_json_content(&formatted, format), max_chars),
        browser,
        os,
    );
    result.site = url::Url::parse(final_url)
        .map(|parsed| parsed.host_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    Ok(result)
}

// ===== main loop (extract.ts:1131-1710, P0 subset) =====

/// Injectable transport (mirrors upstream `FetchDependencies.fetch` in
/// types.ts:172-183): the real wreq client by default; the parity harness
/// and mock-server tests substitute scripted responses/failures so the error
/// templates and pipeline branches compare against the pinned upstream
/// `createDefuddleFetch` under identical conditions. The request moves in —
/// boxed futures must own their data.
pub type TransportFn =
    std::sync::Arc<dyn Fn(HttpRequest) -> futures_boxed::BoxedFetch + Send + Sync>;

/// Support shim so the transport seam needs no extra dependency: a boxed
/// future resolving to the transport result.
pub mod futures_boxed {
    use crate::http::{FetchFailure, HttpResponse};
    use crate::types::FetchOutcome;

    pub type BoxedFetch = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<HttpResponse, FetchFailure>> + Send>,
    >;

    /// Box a concrete transport future.
    pub fn boxed<F>(future: F) -> BoxedFetch
    where
        F: std::future::Future<Output = Result<HttpResponse, FetchFailure>> + Send + 'static,
    {
        Box::pin(future)
    }

    /// Boxed pipeline outcome — the recursion seam for meta-refresh and
    /// alternate-follow hops (a bare `async fn` recursion would need the
    /// future's type to contain itself). Borrowed: the hop re-enters the
    /// same pipeline with a cloned-options request.
    pub type BoxedOutcome<'a> =
        std::pin::Pin<Box<dyn std::future::Future<Output = FetchOutcome> + Send + 'a>>;

    pub fn boxed_outcome<'a, F>(future: F) -> BoxedOutcome<'a>
    where
        F: std::future::Future<Output = FetchOutcome> + Send + 'a,
    {
        Box::pin(future)
    }
}

/// The default transport: the real wreq fingerprint client.
fn default_transport(request: HttpRequest) -> futures_boxed::BoxedFetch {
    async fn fetch_owned(request: HttpRequest) -> Result<crate::http::HttpResponse, FetchFailure> {
        http::fetch(&request).await
    }
    futures_boxed::boxed(fetch_owned(request))
}

// ===== execution hooks (FR-P2-A, types.ts:207-216) =====

/// `FetchProgressUpdate` (types.ts:207-211). `phase` is the pipeline
/// position label every upstream emit site carries (event type name or
/// terminal marker).
#[derive(Debug, Clone)]
pub struct FetchProgressUpdate {
    pub status: &'static str,
    pub progress: f64,
    pub phase: &'static str,
}

/// `FetchExecutionHooks` (types.ts:213-216): the progress-event seam. The
/// execute layer plugs toolUpdate streaming in (FR-P2-B), the parity
/// harness an event collector; `None` hooks (default) keep the pipeline
/// silent. `Arc<dyn Fn>` so recursive hops and batch workers can share one
/// sink.
/// The `onStatusChange` callback shape (types.ts:214).
pub type StatusChangeFn = std::sync::Arc<dyn Fn(&str) + Send + Sync>;
/// The `onProgressChange` callback shape (types.ts:215).
pub type ProgressChangeFn = std::sync::Arc<dyn Fn(&FetchProgressUpdate) + Send + Sync>;

#[derive(Clone, Default)]
pub struct FetchExecutionHooks {
    pub on_status_change: Option<StatusChangeFn>,
    pub on_progress_change: Option<ProgressChangeFn>,
}

impl FetchExecutionHooks {
    /// `emitStatus` (extract.ts:633-637).
    fn emit_status(&self, status: &'static str) {
        if let Some(callback) = &self.on_status_change {
            callback(status);
        }
    }

    /// `emitProgress` (extract.ts:626-630). Crate-visible: the download
    /// branch emits per-chunk `body_progress` frames (FR-P2-A).
    pub(crate) fn emit_progress(&self, status: &'static str, progress: f64, phase: &'static str) {
        if let Some(callback) = &self.on_progress_change {
            callback(&FetchProgressUpdate {
                status,
                progress,
                phase,
            });
        }
    }
}

/// The fetch pipeline with a pluggable extractor and transport. The default
/// engine is `dom_smoothie` ([VARIANT], design §3.2).
pub struct FetchPipeline {
    pub extractor: Box<dyn Extractor>,
    pub transport: TransportFn,
    /// Progress-event sink (FR-P2-A); default silent.
    pub hooks: FetchExecutionHooks,
}

impl Default for FetchPipeline {
    fn default() -> Self {
        FetchPipeline {
            extractor: Box::new(DomSmoothieExtractor),
            transport: std::sync::Arc::new(default_transport),
            hooks: FetchExecutionHooks::default(),
        }
    }
}

impl FetchPipeline {
    /// Default engines with an explicit hook sink (the execute layer's
    /// streaming pipeline; FR-P2-B).
    pub fn with_hooks(hooks: FetchExecutionHooks) -> Self {
        FetchPipeline {
            hooks,
            ..FetchPipeline::default()
        }
    }
}

impl FetchPipeline {
    /// `defuddleFetch` entry (extract.ts:1704-1709): recursion budgets start
    /// at zero, the pipeline's own hook sink serves.
    pub async fn fetch(&self, opts: &crate::types::FetchOptions) -> FetchOutcome {
        self.fetch_with_hooks(opts, &self.hooks).await
    }

    /// `executeFetchToolCall` shape (tool.ts:157-163): an explicit hook sink
    /// overrides the pipeline's — the batch worker pool builds per-item
    /// hooks (upstream passes per-item `FetchExecutionHooks` into
    /// `executeItem`, tool.ts:285-294).
    pub async fn fetch_with_hooks(
        &self,
        opts: &crate::types::FetchOptions,
        hooks: &FetchExecutionHooks,
    ) -> FetchOutcome {
        self.fetch_inner(opts, 0, 0, hooks).await
    }

    /// `fetchWithClientRedirects` with recursion budgets
    /// `client_side_redirect_count` / `alternate_link_fallback_count`
    /// (extract.ts:1134-1138). Boxed recursion: the async fn's self-referential
    /// future would otherwise grow unboundedly per hop.
    fn fetch_inner<'a>(
        &'a self,
        opts: &'a crate::types::FetchOptions,
        client_side_redirect_count: u32,
        alternate_link_fallback_count: u32,
        hooks: &'a FetchExecutionHooks,
    ) -> futures_boxed::BoxedOutcome<'a> {
        futures_boxed::boxed_outcome(async move {
            self.fetch_step(
                opts,
                client_side_redirect_count,
                alternate_link_fallback_count,
                hooks,
            )
            .await
        })
    }

    async fn fetch_step(
        &self,
        opts: &crate::types::FetchOptions,
        client_side_redirect_count: u32,
        alternate_link_fallback_count: u32,
        hooks: &FetchExecutionHooks,
    ) -> FetchOutcome {
        let browser = opts
            .browser
            .clone()
            .unwrap_or_else(|| DEFAULT_BROWSER.to_string());
        let os = opts.os.clone().unwrap_or_else(|| DEFAULT_OS.to_string());
        let format = opts.format.unwrap_or_default();
        let max_chars = opts.max_chars.unwrap_or(DEFAULT_MAX_CHARS);
        let remove_images = opts.remove_images.unwrap_or(false);
        let include_replies = opts
            .include_replies
            .clone()
            .unwrap_or(IncludeReplies::Extractors);
        let timeout_ms = opts.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

        // FR-P0-2 URL validation (extract.ts:1148-1169).
        let parsed = match url::Url::parse(&opts.url) {
            Ok(parsed) => parsed,
            Err(_) => {
                return FetchOutcome::Error(FetchError::validation(
                    format!("Invalid URL: {}", opts.url),
                    crate::types::FetchErrorCode::InvalidUrl,
                    &opts.url,
                ));
            }
        };
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            // JS `parsed.protocol` renders with the trailing colon ("ftp:").
            return FetchOutcome::Error(FetchError::validation(
                format!("Only http/https URLs supported, got {}:", parsed.scheme()),
                crate::types::FetchErrorCode::UnsupportedProtocol,
                &opts.url,
            ));
        }

        // FR-P0-4 headers (extract.ts:1171-1186): Accept by format,
        // Accept-Language default, custom headers override.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "Accept".to_string(),
            resolve_accept_header(format).to_string(),
        );
        headers.insert(
            "Accept-Language".to_string(),
            DEFAULT_ACCEPT_LANGUAGE_HEADER.to_string(),
        );
        if let Some(custom) = &opts.headers {
            for (name, value) in custom {
                headers.insert(name.clone(), value.clone());
            }
        }

        let request = HttpRequest {
            url: opts.url.clone(),
            browser: browser.clone(),
            os: os.clone(),
            headers,
            proxy: opts.proxy.clone(),
            timeout_ms,
        };

        let mut context = FetchErrorContext::new(&opts.url, timeout_ms);

        // FR-P2-A (extract.ts:1194): the pipeline-open progress event.
        // Upstream emits before the transport call; URL/protocol rejections
        // above never reach it (they sit outside the try block).
        hooks.emit_progress("connecting", 0.0, "fetch_start");

        // FR-P0-5/6 transport + status handling (extract.ts:1230-1256).
        let response = match (self.transport)(request).await {
            Ok(response) => response,
            Err(failure) => {
                // extract.ts:1698-1699 (catch): transport throws emit the
                // terminal error pair. return-path errors (below) never do.
                hooks.emit_status("error");
                hooks.emit_progress("error", 1.0, "error");
                return FetchOutcome::Error(map_fetch_failure(&failure, &context));
            }
        };

        // FR-P2-A (TE-D27): the engine event stream (wreq-js
        // onRequestEvent) is unreachable, so response arrival approximates
        // the `response_headers` event (mapRequestEventToProgress). The
        // event also advances the error-context phase (extract.ts:1219-1224:
        // response_headers/body_progress/body_complete → loading).
        context.phase = FetchErrorPhase::Loading;
        hooks.emit_progress("loading", 0.51, "response_headers");

        context.final_url = Some(response.final_url.clone());
        context.status_code = Some(response.status);
        context.status_text = Some(response.status_text.clone());
        context.mime_type = Some(normalize_content_type(
            response.header("content-type").unwrap_or_default(),
        ))
        .filter(|mime| !mime.is_empty());
        context.content_length = parse_content_length_header(response.header("content-length"));

        if !(200..300).contains(&response.status) {
            return FetchOutcome::Error(FetchError {
                error: format!(
                    "Server returned HTTP {} {} for {}.",
                    response.status, response.status_text, opts.url
                ),
                code: Some(crate::types::FetchErrorCode::HttpError),
                phase: Some(context.phase),
                retryable: Some(response.status >= 500 || response.status == 429),
                url: Some(opts.url.clone()),
                final_url: Some(response.final_url.clone()),
                status_code: Some(response.status),
                status_text: Some(response.status_text.clone()),
                timeout_ms: Some(timeout_ms),
                mime_type: context.mime_type.clone(),
                content_length: context.content_length,
                downloaded_bytes: None,
            });
        }

        let final_url = response.final_url.clone();
        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_string();
        let content_disposition = response
            .header("content-disposition")
            .unwrap_or_default()
            .to_string();

        // FR-P1-4 download branch (extract.ts:1262-1286): attachments and
        // non-textual bodies stream to the temp dir, never through
        // extraction.
        let should_download_to_file = is_attachment_disposition(&content_disposition)
            || !is_textual_content_type(&content_type);
        if should_download_to_file {
            context.phase = FetchErrorPhase::Loading;
            // FR-P2-A (TE-D27): per-chunk `body_progress` approximation on
            // the download path only (FR-P2-A scope); the terminal events
            // mirror extract.ts:1278-1283 (`file_done` on success — a file
            // error returns silently) and the catch pair on failure.
            let content_length = context.content_length;
            let outcome = crate::download::build_file_result(
                &opts.url,
                response.body,
                &final_url,
                &content_type,
                &content_disposition,
                &browser,
                &os,
                opts.temp_dir.as_deref(),
                &context,
                hooks,
                content_length,
            )
            .await;
            match &outcome {
                FetchOutcome::Result(_) => {
                    hooks.emit_progress("loading", 0.95, "body_complete");
                    hooks.emit_status("done");
                    hooks.emit_progress("done", 1.0, "file_done");
                }
                FetchOutcome::Error(_) => {
                    hooks.emit_status("error");
                    hooks.emit_progress("error", 1.0, "error");
                }
            }
            return outcome;
        }

        context.phase = FetchErrorPhase::Loading;
        let raw_body = match response.body.read_all().await {
            Ok(body) => body,
            Err(error) => {
                // Body-read failures classify like upstream's transport
                // throws (extract.ts:1696-1701) — the catch pair fires.
                hooks.emit_status("error");
                hooks.emit_progress("error", 1.0, "error");
                let failure = http::FetchFailure::Transport {
                    failure: http::classify_transport_error(&error),
                    final_url: Some(final_url.clone()),
                };
                return FetchOutcome::Error(map_fetch_failure(&failure, &context));
            }
        };

        // FR-P2-A (TE-D27): buffered-read completion approximates the
        // `body_complete` event; the text pipeline is two-valued
        // (0.51 → 0.95, no per-chunk frames).
        hooks.emit_progress("loading", 0.95, "body_complete");

        // FR-P1-2 meta refresh (extract.ts:1290-1312): applies to every
        // format (the check precedes the raw/json branches upstream).
        if let Some(redirect_target) = extract_client_side_redirect(&raw_body, &final_url) {
            if client_side_redirect_count >= MAX_CLIENT_SIDE_REDIRECTS {
                return FetchOutcome::Error(FetchError {
                    error: format!(
                        "Client-side redirect limit ({MAX_CLIENT_SIDE_REDIRECTS}) exceeded while fetching {}.",
                        opts.url
                    ),
                    code: Some(crate::types::FetchErrorCode::TooManyRedirects),
                    phase: Some(FetchErrorPhase::Loading),
                    retryable: Some(false),
                    timeout_ms: Some(timeout_ms),
                    url: Some(opts.url.clone()),
                    final_url: Some(final_url.clone()),
                    status_code: None,
                    status_text: None,
                    mime_type: Some(normalize_content_type(&content_type))
                        .filter(|mime| !mime.is_empty()),
                    content_length: context.content_length,
                    downloaded_bytes: None,
                });
            }
            return self
                .fetch_inner(
                    &opts.with_url(redirect_target),
                    client_side_redirect_count + 1,
                    alternate_link_fallback_count,
                    hooks,
                )
                .await;
        }

        let json_response = is_json_response(&content_type, &raw_body);

        // FR-P0-8 raw format (extract.ts:1316-1404). The X/Twitter oEmbed
        // deleted-tweet probe is P2 [DEFER] (requirements §2.3).
        if format == OutputFormat::Raw {
            let effective_content = if opts.max_chars.is_some() {
                truncate_content(&raw_body, max_chars)
            } else {
                raw_body.clone()
            };
            let mut result =
                FetchResult::content(&opts.url, &final_url, 0, effective_content, &browser, &os);
            result.site = url::Url::parse(&final_url)
                .map(|parsed| parsed.host_str().unwrap_or_default().to_string())
                .unwrap_or_default();
            result.content_type =
                Some(normalize_content_type(&content_type)).filter(|mime| !mime.is_empty());
            hooks.emit_status("done");
            hooks.emit_progress("done", 1.0, "raw_done");
            return FetchOutcome::Result(result);
        }

        if format == OutputFormat::Json {
            if !json_response {
                // FR-P1-3 (extract.ts:1406-1437): an HTML body may carry a
                // qualified alternate link worth following before the
                // terminal error.
                if HTML_CONTENT_TYPES
                    .iter()
                    .any(|value| content_type.contains(value))
                {
                    // scraper's `Html` is not `Send` (non-atomic tendril
                    // counters) — finish the DOM work and drop the document
                    // before the recursive await.
                    let alternate_links = {
                        let document = Html::parse_document(&raw_body);
                        extract_qualified_alternate_links(&document, &final_url, format)
                    };
                    if !alternate_links.is_empty()
                        && alternate_link_fallback_count < MAX_ALTERNATE_LINK_FALLBACKS
                    {
                        return self
                            .fetch_inner(
                                &opts.with_url(alternate_links[0].clone()),
                                client_side_redirect_count,
                                alternate_link_fallback_count + 1,
                                hooks,
                            )
                            .await;
                    }
                }

                return FetchOutcome::Error(FetchError {
                    error: format!("Not a JSON response (content-type: {content_type})"),
                    code: Some(crate::types::FetchErrorCode::UnexpectedResponse),
                    phase: Some(context.phase),
                    retryable: Some(false),
                    timeout_ms: Some(timeout_ms),
                    url: Some(opts.url.clone()),
                    final_url: Some(final_url.clone()),
                    status_code: None,
                    status_text: None,
                    mime_type: Some(normalize_content_type(&content_type))
                        .filter(|mime| !mime.is_empty()),
                    content_length: context.content_length,
                    downloaded_bytes: None,
                });
            }
            return match build_json_result(
                &opts.url, &final_url, &raw_body, format, max_chars, &browser, &os,
            ) {
                Ok(result) => {
                    hooks.emit_status("done");
                    hooks.emit_progress("done", 1.0, "json_done");
                    FetchOutcome::Result(result)
                }
                Err(error) => FetchOutcome::Error(error),
            };
        }

        if json_response {
            // extract.ts:1460-1479: JSON bodies render regardless of format.
            return match build_json_result(
                &opts.url, &final_url, &raw_body, format, max_chars, &browser, &os,
            ) {
                Ok(result) => {
                    hooks.emit_status("done");
                    hooks.emit_progress("done", 1.0, "json_done");
                    FetchOutcome::Result(result)
                }
                Err(error) => FetchOutcome::Error(error),
            };
        }

        if is_plain_text_content_type(&content_type) {
            hooks.emit_status("done");
            hooks.emit_progress("done", 1.0, "plain_text_done");
            return FetchOutcome::Result(build_plain_text_result(
                &opts.url, &final_url, &raw_body, format, max_chars, &browser, &os,
            ));
        }

        if !HTML_CONTENT_TYPES
            .iter()
            .any(|value| content_type.contains(value))
        {
            return FetchOutcome::Error(FetchError {
                error: format!("Not an HTML page (content-type: {content_type})"),
                code: Some(crate::types::FetchErrorCode::UnexpectedResponse),
                phase: Some(context.phase),
                retryable: Some(false),
                timeout_ms: Some(timeout_ms),
                url: Some(opts.url.clone()),
                final_url: Some(final_url.clone()),
                status_code: None,
                status_text: None,
                mime_type: Some(normalize_content_type(&content_type))
                    .filter(|mime| !mime.is_empty()),
                content_length: context.content_length,
                downloaded_bytes: None,
            });
        }

        // FR-P0-7 extraction (extract.ts:1514-1643) with the FR-P1-3 alternate
        // fallbacks (extract.ts:1521-1543, 1617-1655).
        context.phase = FetchErrorPhase::Processing;
        // extract.ts:1515-1520: the extracting pair precedes the DOM work.
        hooks.emit_status("processing");
        hooks.emit_progress("processing", 0.96, "extracting");
        // All DOM work happens up front: scraper's `Html` is not `Send`
        // (non-atomic tendril counters), so the documents are dropped before
        // any recursive await. Upstream parses two same-source documents
        // (`extractionDocument` feeds Defuddle; the Rust adapter consumes the
        // raw HTML string directly, so only the fallback document exists).
        let (alternate_links, fallback_text, fallback_markdown) = {
            let fallback_document = Html::parse_document(&raw_body);
            let alternate_links =
                extract_qualified_alternate_links(&fallback_document, &final_url, format);
            let fallback_text = crate::extract::extract_dom_text_fallback(&fallback_document);
            let fallback_markdown = if format == OutputFormat::Markdown {
                crate::extract::extract_dom_markdown_fallback(&fallback_document)
            } else {
                String::new()
            };
            (alternate_links, fallback_text, fallback_markdown)
        };

        // `tryAlternateLinkFallback` (extract.ts:1529-1543).
        let try_alternate = || async {
            if alternate_links.is_empty()
                || alternate_link_fallback_count >= MAX_ALTERNATE_LINK_FALLBACKS
            {
                return None;
            }
            Some(
                self.fetch_inner(
                    &opts.with_url(alternate_links[0].clone()),
                    client_side_redirect_count,
                    alternate_link_fallback_count + 1,
                    hooks,
                )
                .await,
            )
        };

        let extracted = self.extractor.extract(
            &raw_body,
            &final_url,
            &ExtractOptions {
                markdown: format != OutputFormat::Html,
                remove_images,
                include_replies: include_replies.clone(),
            },
        );

        let (mut extracted_content, mut word_count) =
            if extracted.content.as_deref().unwrap_or("").is_empty() || extracted.word_count == 0 {
                if fallback_text.is_empty() {
                    // extract.ts:1619-1621: alternate fallback before the
                    // no_content terminal.
                    if let Some(alternate_result) = try_alternate().await {
                        return alternate_result;
                    }
                    return FetchOutcome::Error(FetchError {
                        error: format!(
                            "No content extracted from {}. May need JS rendering or is blocked.",
                            opts.url
                        ),
                        code: Some(crate::types::FetchErrorCode::NoContent),
                        phase: Some(FetchErrorPhase::Processing),
                        retryable: Some(false),
                        timeout_ms: Some(timeout_ms),
                        url: Some(opts.url.clone()),
                        final_url: Some(final_url.clone()),
                        status_code: None,
                        status_text: None,
                        mime_type: Some(normalize_content_type(&content_type))
                            .filter(|mime| !mime.is_empty()),
                        content_length: context.content_length,
                        downloaded_bytes: None,
                    });
                }
                let content = match format {
                    OutputFormat::Html => raw_body.clone(),
                    OutputFormat::Markdown => {
                        if fallback_markdown.is_empty() {
                            fallback_text.clone()
                        } else {
                            fallback_markdown.clone()
                        }
                    }
                    _ => fallback_text.clone(),
                };
                (content, estimate_word_count(&fallback_text))
            } else {
                (extracted.content.unwrap_or_default(), extracted.word_count)
            };

        // Thin-content alternate check (extract.ts:1645-1655): follow when
        // min(engine wordCount, rendered word count) drops below 30 — and
        // keep the thin result when there is nothing (left) to follow.
        let extracted_text_word_count = if format == OutputFormat::Text {
            estimate_word_count(&extracted_content)
        } else {
            estimate_word_count(&markdown_to_text(&extracted_content))
        };
        if std::cmp::min(word_count, extracted_text_word_count)
            < MIN_EXTRACTED_WORDS_BEFORE_ALTERNATE_FALLBACK
            && !alternate_links.is_empty()
        {
            if let Some(alternate_result) = try_alternate().await {
                return alternate_result;
            }
        }

        // includeReplies=false comment stripping (extract.ts:1657-1673) —
        // only meaningful when the engine emits comment markers; the
        // strip function itself is byte-parity tested in format.rs.
        if include_replies == IncludeReplies::None
            && should_strip_replies(&extracted.site.clone().unwrap_or_default())
        {
            let stripped = crate::format::strip_extractor_comments(&extracted_content, format);
            if stripped != extracted_content {
                extracted_content = stripped;
                word_count = if format == OutputFormat::Text {
                    estimate_word_count(&markdown_to_text(&extracted_content))
                } else {
                    estimate_word_count(&extracted_content)
                };
            }
        }

        let normalized_content = if format == OutputFormat::Text {
            markdown_to_text(&extracted_content)
        } else {
            extracted_content.clone()
        };

        let mut result = FetchResult::content(
            &opts.url,
            &final_url,
            word_count,
            truncate_content(&normalized_content, max_chars),
            &browser,
            &os,
        );
        result.title = extracted.title.clone().unwrap_or_default();
        result.author = extracted.author.clone().unwrap_or_default();
        result.published = extracted.published.clone().unwrap_or_default();
        result.site = extracted.site.clone().unwrap_or_default();
        result.language = extracted.language.clone().unwrap_or_default();
        // extract.ts:1693-1694: the extraction-success terminal pair.
        hooks.emit_status("done");
        hooks.emit_progress("done", 1.0, "done");
        FetchOutcome::Result(result)
    }
}

/// `shouldStripReplies` (extract.ts:1119-1125).
fn should_strip_replies(site: &str) -> bool {
    site == "Hacker News" || site.starts_with("r/") || site.starts_with("GitHub - ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_attribute_entities_decode_case_insensitively() {
        // extract.ts:922-929 uses /gi — `&AMP;` decodes like `&amp;`.
        assert_eq!(decode_html_attribute("&AMP;"), "&");
        assert_eq!(decode_html_attribute("&QuOt;"), "\"");
        assert_eq!(decode_html_attribute("&#39;"), "'");
        assert_eq!(decode_html_attribute("&APOS;"), "'");
        assert_eq!(decode_html_attribute("a&lt;b&amp;&GT;c"), "a<b&>c");
    }

    #[test]
    fn content_type_classification() {
        assert!(is_textual_content_type("text/html; charset=utf-8"));
        assert!(is_textual_content_type("application/ld+json"));
        assert!(is_textual_content_type("IMAGE/SVG+XML"));
        assert!(!is_textual_content_type("application/octet-stream"));
        assert!(is_plain_text_content_type("text/plain; charset=utf-8"));
        assert!(!is_plain_text_content_type("text/html"));
        assert!(is_json_content_type("application/json; charset=utf-8"));
        assert!(is_json_content_type("application/vnd.api+json"));
        assert!(!is_json_content_type("text/html"));
    }

    #[test]
    fn attachment_disposition() {
        assert!(is_attachment_disposition("attachment"));
        assert!(is_attachment_disposition("ATTACHMENT; filename=\"x\""));
        assert!(!is_attachment_disposition("inline; filename=\"x\""));
    }

    #[test]
    fn meta_refresh_detection_boundaries() {
        let page = "<html><head><meta http-equiv=\"refresh\" content=\"0;url=/next.html\"></head><body>x</body></html>";
        assert_eq!(
            extract_client_side_redirect(page, "https://example.com/start"),
            Some("https://example.com/next.html".to_string())
        );
        // delay >= 30 → no redirect
        let slow = "<meta http-equiv=\"refresh\" content=\"30;url=/next.html\">";
        assert_eq!(
            extract_client_side_redirect(slow, "https://example.com/"),
            None
        );
        // self-target → no redirect
        let self_target = "<meta http-equiv=\"refresh\" content=\"0;url=https://example.com/\">";
        assert_eq!(
            extract_client_side_redirect(self_target, "https://example.com/"),
            None
        );
        // no meta at all
        assert_eq!(
            extract_client_side_redirect("<html></html>", "https://example.com/"),
            None
        );
    }

    #[test]
    fn alternate_link_extraction_by_format() {
        let html = "<html><head>\
            <link rel=\"alternate\" type=\"text/markdown\" href=\"/md\">\
            <link rel=\"alternate\" type=\"application/json\" href=\"/api\">\
            <link rel=\"alternate\" type=\"application/rss+xml\" href=\"/rss\">\
            <link rel=\"stylesheet\" href=\"/style.css\">\
            <link rel=\"alternate\" type=\"text/markdown\" href=\"/md\">\
            </head><body></body></html>";
        let document = Html::parse_document(html);
        let markdown_links = extract_qualified_alternate_links(
            &document,
            "https://example.com/post",
            OutputFormat::Markdown,
        );
        assert_eq!(markdown_links, vec!["https://example.com/md".to_string()]);
        let json_links = extract_qualified_alternate_links(
            &document,
            "https://example.com/post",
            OutputFormat::Json,
        );
        assert_eq!(json_links, vec!["https://example.com/api".to_string()]);
        let raw_links = extract_qualified_alternate_links(
            &document,
            "https://example.com/post",
            OutputFormat::Raw,
        );
        assert!(raw_links.is_empty());
    }

    #[test]
    fn content_length_header_parsing() {
        assert_eq!(parse_content_length_header(Some("123")), Some(123));
        assert_eq!(parse_content_length_header(Some("123abc")), Some(123));
        assert_eq!(parse_content_length_header(Some("abc")), None);
        assert_eq!(parse_content_length_header(Some("-1")), None);
        assert_eq!(parse_content_length_header(None), None);
    }

    #[test]
    fn timeout_error_templates() {
        let mut context = FetchErrorContext::new("https://example.com/", 15000);
        context.final_url = Some("https://example.com/final".to_string());
        let error = build_timeout_error(&context);
        assert_eq!(
            error.error,
            "Timeout of 15000ms exceeded while connecting to https://example.com/final."
        );

        context.phase = FetchErrorPhase::Loading;
        context.content_length = Some(10240);
        context.mime_type = Some("application/zip".to_string());
        let error = build_timeout_error(&context);
        assert_eq!(
            error.error,
            "Timeout of 15000ms exceeded while downloading a 10.0 KB file from https://example.com/final."
        );
    }

    #[test]
    fn thrown_error_templates() {
        let context = FetchErrorContext::new("https://no-such-host.example/", 15000);
        let error = build_thrown_fetch_error(&http::TransportFailure::Dns, &context);
        assert_eq!(
            error.error,
            "DNS error: failed to lookup address for no-such-host.example. Check the URL and try again."
        );
        assert_eq!(error.retryable, Some(false));

        let error = build_thrown_fetch_error(
            &http::TransportFailure::Other {
                message: "Invalid browser profile: chrome_99. Available profiles: …".to_string(),
            },
            &context,
        );
        assert!(error.error.starts_with(
            "Request failed while connecting for https://no-such-host.example/: Invalid browser profile: chrome_99"
        ));
    }

    #[tokio::test]
    async fn invalid_url_and_protocol_rejections() {
        let pipeline = FetchPipeline::default();
        let opts = crate::types::FetchOptions {
            url: "not a url".to_string(),
            browser: None,
            os: None,
            headers: None,
            format: None,
            max_chars: None,
            remove_images: None,
            include_replies: None,
            proxy: None,
            timeout_ms: None,
            temp_dir: None,
        };
        match pipeline.fetch(&opts).await {
            FetchOutcome::Error(error) => {
                assert_eq!(error.error, "Invalid URL: not a url");
                assert_eq!(error.code.map(|c| c.as_str()), Some("invalid_url"));
            }
            FetchOutcome::Result(_) => panic!("expected error"),
        }

        let opts = crate::types::FetchOptions {
            url: "ftp://example.com/file".to_string(),
            ..opts
        };
        match pipeline.fetch(&opts).await {
            FetchOutcome::Error(error) => {
                assert_eq!(error.error, "Only http/https URLs supported, got ftp:");
            }
            FetchOutcome::Result(_) => panic!("expected error"),
        }
    }
}
