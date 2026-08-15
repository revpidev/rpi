//! Pure formatting functions mirroring upstream `packages/core/src/format.ts`
//! @ b0111612 — the byte-parity battleground for TE06 (design §5.1).
//!
//! Every template, join order and omission rule is 1:1; the two JS-specific
//! behaviors that Rust does not share are reproduced deliberately:
//! - `Number.prototype.toFixed` rounds halves away from zero on the exact
//!   decimal expansion of the double (ECMA-262 §6.1.6.1.20 picks the larger
//!   `n` on ties), while Rust's `{:.N}` formats round-half-even — `js_to_fixed`
//!   implements the JS rule;
//! - `String.prototype.trimEnd` trims U+FEFF (JS WhiteSpace includes BOM),
//!   Rust `char::is_whitespace` does not — `trim_end` keeps parity by
//!   trimming FEFF as well.

use std::sync::LazyLock;

use regex::Regex;

use crate::types::{FetchError, FetchErrorPhase, FetchResult, OutputFormat};

/// `Number.prototype.toFixed(decimals)` (ECMA-262: nearest n to the exact
/// decimal expansion of the double, ties to the larger n).
///
/// Rust's `{:.N}` also rounds the exact decimal expansion of the double but
/// breaks ties to even, so exact ties (the double's value is precisely
/// `m + 0.5` scaled by 10^d) are detected via the bit decomposition
/// `value = M·2^e` — tie ⟺ `trailing_zeros(M) + e + d == -1` — and rounded
/// up through integer arithmetic. Multiplying by 10^d first is NOT faithful:
/// `2.675f64 * 100.0 == 267.5` in f64 while the double's exact value rounds
/// to "2.67" in JS.
pub fn js_to_fixed(value: f64, decimals: usize) -> String {
    if let Some(floor) = exact_tie_floor(value, decimals) {
        let half_up = (floor + 1) as f64 / 10f64.powi(decimals as i32);
        return format!("{:.*}", decimals, half_up);
    }
    format!("{:.*}", decimals, value)
}

/// When `value · 10^decimals` is exactly `m + 0.5`, returns `Some(m)`.
fn exact_tie_floor(value: f64, decimals: usize) -> Option<u64> {
    let bits = value.to_bits();
    if bits >> 63 != 0 {
        return None; // non-negative domain only (byte counts, durations)
    }
    let biased = ((bits >> 52) & 0x7ff) as i64;
    let mantissa = bits & 0x000f_ffff_ffff_ffff;
    let (m, e) = if biased == 0 {
        if mantissa == 0 {
            return None;
        }
        (mantissa, -1074i64)
    } else {
        (mantissa | (1u64 << 52), biased - 1023 - 52)
    };
    let trailing = m.trailing_zeros() as i64;
    if trailing + e + decimals as i64 != -1 {
        return None;
    }
    let odd_part = m >> trailing;
    // value·10^d = odd_part·5^d / 2 — odd numerator ⇒ exactly x.5.
    let numerator = odd_part.checked_mul(5u64.pow(decimals as u32))?;
    Some((numerator - 1) / 2)
}

/// JS `str.trimEnd()` — trims WhiteSpace + LineTerminator + U+FEFF.
fn trim_end_js(value: &str) -> &str {
    value.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `buildHeader` (format.ts:16-23): `> Label: value` lines joined by \n;
/// undefined and empty-string values are omitted (numbers, including 0, are
/// kept — JS `0 !== ""` is true across types).
pub enum HeaderValue {
    Text(String),
    Number(u64),
}

impl HeaderValue {
    fn render(&self) -> Option<String> {
        match self {
            HeaderValue::Text(value) => {
                if value.is_empty() {
                    None
                } else {
                    Some(value.clone())
                }
            }
            HeaderValue::Number(value) => Some(value.to_string()),
        }
    }
}

impl From<&str> for HeaderValue {
    fn from(value: &str) -> Self {
        HeaderValue::Text(value.to_string())
    }
}

impl From<String> for HeaderValue {
    fn from(value: String) -> Self {
        HeaderValue::Text(value)
    }
}

impl From<u64> for HeaderValue {
    fn from(value: u64) -> Self {
        HeaderValue::Number(value)
    }
}

pub fn build_header(parts: &[(&str, HeaderValue)]) -> String {
    parts
        .iter()
        .filter_map(|(label, value)| {
            value
                .render()
                .map(|rendered| format!("> {label}: {rendered}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `formatByteCount` (format.ts:25-37; duplicated at extract.ts:640-652).
/// bytes / 1024^k is exact in f64 (power-of-two divisor), so the JS toFixed
/// parity above holds exactly here.
pub fn format_byte_count(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    let decimals = if unit_index == 0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    format!("{} {}", js_to_fixed(value, decimals), UNITS[unit_index])
}

/// `formatDurationMs` (format.ts:39-51).
pub fn format_duration_ms(duration_ms: u64) -> String {
    if duration_ms < 1000 {
        return format!("{duration_ms}ms");
    }
    let seconds = duration_ms as f64 / 1000.0;
    if seconds < 60.0 {
        let fixed = if seconds >= 10.0 { 0 } else { 1 };
        return format!("{duration_ms}ms ({}s)", js_to_fixed(seconds, fixed));
    }
    let minutes = seconds / 60.0;
    let fixed = if minutes >= 10.0 { 0 } else { 1 };
    format!("{duration_ms}ms ({}m)", js_to_fixed(minutes, fixed))
}

/// `describeErrorPhase` (format.ts:53-68).
pub fn describe_error_phase(phase: Option<FetchErrorPhase>) -> &'static str {
    match phase {
        Some(FetchErrorPhase::Validation) => "validating the request",
        Some(FetchErrorPhase::Connecting) => "connecting",
        Some(FetchErrorPhase::Waiting) => "waiting for the server response",
        Some(FetchErrorPhase::Loading) => "downloading the response body",
        Some(FetchErrorPhase::Processing) => "processing the response",
        _ => "unknown",
    }
}

/// `roundSuggestedTimeoutMs` (format.ts:70-75): ceil onto the 1s/5s/10s/30s
/// ladder. JS `Math.ceil(a / b) * b` on f64; inputs here stay well within
/// exact-integer f64 range.
fn round_suggested_timeout_ms(value: f64) -> u64 {
    let step = if value <= 10_000.0 {
        1_000.0
    } else if value <= 60_000.0 {
        5_000.0
    } else if value <= 300_000.0 {
        10_000.0
    } else {
        30_000.0
    };
    (value / step).ceil() as u64 * step as u64
}

/// `suggestRetryTimeoutMs` (format.ts:77-102).
pub fn suggest_retry_timeout_ms(error: &FetchError) -> Option<u64> {
    let timeout_ms = error.timeout_ms?;
    if timeout_ms == 0 {
        return None;
    }
    let phase = error.phase.unwrap_or_default();
    if error.phase == Some(FetchErrorPhase::Loading) {
        // JS truthiness: `error.contentLength && error.downloadedBytes &&`
        // — 0 values fall through to the phase branches below.
        if let (Some(content_length), Some(downloaded_bytes)) =
            (error.content_length, error.downloaded_bytes)
        {
            if downloaded_bytes > 0 && content_length > 0 {
                let projected_ms =
                    (timeout_ms as f64 * content_length as f64) / downloaded_bytes as f64;
                return Some(round_suggested_timeout_ms(projected_ms * 1.5));
            }
        }
    }
    if phase == FetchErrorPhase::Processing {
        return Some(round_suggested_timeout_ms(timeout_ms as f64 * 2.0));
    }
    if phase == FetchErrorPhase::Connecting || phase == FetchErrorPhase::Waiting {
        return Some(round_suggested_timeout_ms(
            (timeout_ms as f64 * 2.0).max(30_000.0),
        ));
    }
    Some(round_suggested_timeout_ms(timeout_ms as f64 * 2.0))
}

/// `buildUserFacingFetchErrorSummary` (format.ts:104-154) — the short TUI
/// summary line.
pub fn build_user_facing_fetch_error_summary(error: &FetchError) -> String {
    if error
        .code
        .map(|c| c == crate::types::FetchErrorCode::HttpError)
        == Some(true)
    {
        if let Some(status_code) = error.status_code {
            let status_text = error
                .status_text
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| format!(" {t}"))
                .unwrap_or_default();
            return format!("Server responded with {status_code}{status_text}");
        }
    }

    let code = error.code;
    let code = match code {
        Some(code) => code,
        None => return error.error.clone(),
    };
    match code {
        crate::types::FetchErrorCode::InvalidUrl => "That URL is invalid.".to_string(),
        crate::types::FetchErrorCode::UnsupportedProtocol => {
            "Only http and https URLs are supported.".to_string()
        }
        crate::types::FetchErrorCode::Timeout => match error.phase {
            Some(FetchErrorPhase::Connecting) => {
                "Timed out while connecting to the server.".to_string()
            }
            Some(FetchErrorPhase::Waiting) => {
                "The server took too long to start responding.".to_string()
            }
            Some(FetchErrorPhase::Loading) => {
                let is_file = error
                    .mime_type
                    .as_deref()
                    .map(|m| !m.starts_with("text/"))
                    .unwrap_or(false);
                if is_file {
                    "Timed out while downloading the file.".to_string()
                } else {
                    "Timed out while downloading the response.".to_string()
                }
            }
            Some(FetchErrorPhase::Processing) => {
                "Timed out while processing the response.".to_string()
            }
            _ => "The request timed out.".to_string(),
        },
        crate::types::FetchErrorCode::UnexpectedResponse => {
            "The response format was unexpected.".to_string()
        }
        crate::types::FetchErrorCode::DownloadError => {
            "The file could not be saved locally.".to_string()
        }
        crate::types::FetchErrorCode::NoContent => {
            "No readable content could be extracted from the page.".to_string()
        }
        crate::types::FetchErrorCode::ProcessingError => {
            "The response could not be processed.".to_string()
        }
        crate::types::FetchErrorCode::NetworkError => {
            static RE_DNS: LazyLock<Regex> =
                LazyLock::new(|| Regex::new("(?i)dns error").expect("valid regex"));
            static RE_CONNECT: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new("(?i)connection failed|connection refused|unreachable")
                    .expect("valid regex")
            });
            static RE_TLS: LazyLock<Regex> =
                LazyLock::new(|| Regex::new("(?i)tls|ssl").expect("valid regex"));
            if RE_DNS.is_match(&error.error) {
                return "DNS error — could not resolve the hostname.".to_string();
            }
            if RE_CONNECT.is_match(&error.error) {
                return "Connection failed — the server is unreachable.".to_string();
            }
            if RE_TLS.is_match(&error.error) {
                return "TLS/SSL error — certificate may be invalid.".to_string();
            }
            "The request failed before a usable response was returned.".to_string()
        }
        _ => error.error.clone(),
    }
}

/// `buildFetchErrorResponseText` (format.ts:156-236) — the agent-facing error
/// text: `Error: <error>` + conditional metadata header + suggestion lines.
pub fn build_fetch_error_response_text(error: &FetchError) -> String {
    let code = error.code;
    let mut lines = vec![format!("Error: {}", error.error)];

    let with_metadata = code == Some(crate::types::FetchErrorCode::Timeout)
        || code == Some(crate::types::FetchErrorCode::HttpError)
        || code == Some(crate::types::FetchErrorCode::DownloadError);
    if with_metadata {
        let metadata = build_header(&[
            ("URL", opt_text(error.url.as_deref())),
            ("Final URL", opt_text(error.final_url.as_deref())),
            (
                "Phase",
                error
                    .phase
                    .map(|phase| HeaderValue::from(describe_error_phase(Some(phase))))
                    .unwrap_or_else(|| HeaderValue::from("")),
            ),
            (
                "Timeout",
                error
                    .timeout_ms
                    .map(|ms| HeaderValue::from(format_duration_ms(ms)))
                    .unwrap_or_else(|| HeaderValue::from("")),
            ),
            (
                "HTTP status",
                match error.status_code {
                    Some(status_code) => {
                        let status_text = error
                            .status_text
                            .as_deref()
                            .filter(|t| !t.is_empty())
                            .map(|t| format!(" {t}"))
                            .unwrap_or_default();
                        HeaderValue::from(format!("{status_code}{status_text}"))
                    }
                    None => HeaderValue::from(""),
                },
            ),
            ("Mime type", opt_text(error.mime_type.as_deref())),
            (
                "Content-Length",
                error
                    .content_length
                    .map(|len| {
                        HeaderValue::from(format!("{len} bytes ({})", format_byte_count(len)))
                    })
                    .unwrap_or_else(|| HeaderValue::from("")),
            ),
            (
                "Downloaded before failure",
                error
                    .downloaded_bytes
                    .map(|len| {
                        HeaderValue::from(format!("{len} bytes ({})", format_byte_count(len)))
                    })
                    .unwrap_or_else(|| HeaderValue::from("")),
            ),
            (
                "Suggested timeoutMs",
                if code == Some(crate::types::FetchErrorCode::Timeout) {
                    suggest_retry_timeout_ms(error)
                        .map(HeaderValue::from)
                        .unwrap_or_else(|| HeaderValue::from(""))
                } else {
                    HeaderValue::from("")
                },
            ),
        ]);
        if !metadata.is_empty() {
            lines.push(String::new());
            lines.push(metadata);
        }
    }

    if code == Some(crate::types::FetchErrorCode::Timeout) {
        lines.push(String::new());
        lines.push(
            "The timeoutMs parameter is configurable. Retry this call with a higher timeoutMs value."
                .to_string(),
        );
    } else if code == Some(crate::types::FetchErrorCode::HttpError) {
        if error.status_code == Some(429) {
            lines.push(String::new());
            lines.push(
                "The server rate-limited this request. Retrying later or using a different proxy may help."
                    .to_string(),
            );
        } else if error.status_code == Some(401) || error.status_code == Some(403) {
            lines.push(String::new());
            lines.push(
                "The server rejected this request. Authentication, a different browser profile, or a different proxy may be required."
                    .to_string(),
            );
        } else if error.status_code.unwrap_or(0) >= 500 {
            lines.push(String::new());
            lines.push(
                "The server failed while processing the request. Retrying later may help."
                    .to_string(),
            );
        }
    } else if code == Some(crate::types::FetchErrorCode::DownloadError) {
        lines.push(String::new());
        lines.push(
            "The download failed before completion. Retrying may help, especially if the connection was interrupted."
                .to_string(),
        );
    } else if error.retryable == Some(true) {
        lines.push(String::new());
        lines.push("Retrying this request may help.".to_string());
    }

    lines.join("\n")
}

fn opt_text(value: Option<&str>) -> HeaderValue {
    HeaderValue::Text(value.unwrap_or("").to_string())
}

/// `markdownToText` (format.ts:238-248). JS `\s` includes U+FEFF and JS
/// multiline `^` also matches after \r/ /  — both differ from Rust
/// defaults; parity cases live in the byte-parity fixtures.
static RE_MD_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^#{1,6}\s+").expect("valid regex"));
static RE_MD_BOLD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*([^*]+)\*\*").expect("valid regex"));
static RE_MD_ITALIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*([^*]+)\*").expect("valid regex"));
static RE_MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]+\)").expect("valid regex"));
static RE_MD_IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[[^\]]*\]\([^)]+\)").expect("valid regex"));
static RE_MD_QUOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^>\s+").expect("valid regex"));
static RE_MD_LIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[-*+]\s+").expect("valid regex"));
static RE_MD_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([^`]+)`").expect("valid regex"));

pub fn markdown_to_text(markdown: &str) -> String {
    let text = RE_MD_HEADING.replace_all(markdown, "").to_string();
    let text = RE_MD_BOLD.replace_all(&text, "${1}").to_string();
    let text = RE_MD_ITALIC.replace_all(&text, "${1}").to_string();
    let text = RE_MD_LINK.replace_all(&text, "${1}").to_string();
    let text = RE_MD_IMAGE.replace_all(&text, "").to_string();
    let text = RE_MD_QUOTE.replace_all(&text, "").to_string();
    let text = RE_MD_LIST.replace_all(&text, "• ").to_string();
    RE_MD_CODE.replace_all(&text, "${1}").to_string()
}

/// `truncateContent` (format.ts:250-253). JS slices UTF-16 code units; this
/// port truncates on char boundaries instead — declared [VARIANT] (FR-P0-9),
/// astral-plane samples land in the fixtures to record the drift.
pub fn truncate_content(content: &str, max_chars: u64) -> String {
    let max_chars = max_chars as usize;
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let prefix: String = content.chars().take(max_chars).collect();
    format!("{prefix}\n\n[... truncated]")
}

/// `buildCompactMetadataHeader` (format.ts:255-272) — non-verbose header.
pub fn build_compact_metadata_header(result: &FetchResult) -> String {
    if result.is_file() {
        return build_header(&[
            ("URL", HeaderValue::from(result.final_url.as_str())),
            (
                "File size",
                result
                    .file_size
                    .map(HeaderValue::from)
                    .unwrap_or_else(|| HeaderValue::from("")),
            ),
            ("Mime type", opt_text(result.mime_type.as_deref())),
            ("File path", opt_text(result.file_path.as_deref())),
        ]);
    }
    build_header(&[
        ("URL", HeaderValue::from(result.final_url.as_str())),
        ("Title", HeaderValue::from(result.title.as_str())),
        ("Author", HeaderValue::from(result.author.as_str())),
        ("Published", HeaderValue::from(result.published.as_str())),
        ("Content-Type", opt_text(result.content_type.as_deref())),
    ])
}

/// `buildMetadataHeader` (format.ts:274-296) — verbose (full) header.
pub fn build_metadata_header(result: &FetchResult) -> String {
    if result.is_file() {
        return build_header(&[
            ("URL", HeaderValue::from(result.final_url.as_str())),
            (
                "File size",
                result
                    .file_size
                    .map(HeaderValue::from)
                    .unwrap_or_else(|| HeaderValue::from("")),
            ),
            ("Mime type", opt_text(result.mime_type.as_deref())),
            ("File path", opt_text(result.file_path.as_deref())),
            (
                "Browser",
                HeaderValue::from(format!("{}/{}", result.browser, result.os)),
            ),
        ]);
    }
    build_header(&[
        ("URL", HeaderValue::from(result.final_url.as_str())),
        ("Title", HeaderValue::from(result.title.as_str())),
        ("Author", HeaderValue::from(result.author.as_str())),
        ("Published", HeaderValue::from(result.published.as_str())),
        ("Content-Type", opt_text(result.content_type.as_deref())),
        ("Site", HeaderValue::from(result.site.as_str())),
        ("Language", HeaderValue::from(result.language.as_str())),
        ("Words", HeaderValue::from(result.word_count)),
        (
            "Browser",
            HeaderValue::from(format!("{}/{}", result.browser, result.os)),
        ),
    ])
}

/// `buildFetchResponseText` (format.ts:298-311).
pub fn build_fetch_response_text(result: &FetchResult, verbose: bool) -> String {
    let header = if verbose {
        build_metadata_header(result)
    } else {
        build_compact_metadata_header(result)
    };
    if result.is_file() {
        return header;
    }
    if header.is_empty() {
        result.content.clone()
    } else {
        format!("{header}\n\n{}", result.content)
    }
}

/// `estimateWordCount` (format.ts:363-366): `content.trim().match(/\S+/g)`.
pub fn estimate_word_count(content: &str) -> u64 {
    content.split_whitespace().count() as u64
}

/// `escapeHtml` (format.ts:368-375).
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// `parseAndFormatJson` (format.ts:377-387): pretty-print with 2-space
/// indent; parse failure yields the fixed `Invalid JSON response` error.
/// Rendering goes through the JS-semantics renderer below — serde_json's own
/// number formatting (`1e21`, `1e-6`, `100.0`) differs from
/// `JSON.stringify`'s (`1e+21`, `0.000001`, `100`).
pub fn parse_and_format_json(raw: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| "Invalid JSON response".to_string())?;
    Ok(js_stringify_pretty(&value, 0))
}

/// `JSON.stringify(value, null, 2)` — 2-space indent, `": "` after keys,
/// JS number formatting. Known accepted divergence: `JSON.parse` accepts
/// `1e999` (→ Infinity → stringifies to `null`) while serde_json rejects
/// out-of-range floats; no fixture exercises it.
fn js_stringify_pretty(value: &serde_json::Value, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let child_indent = "  ".repeat(depth + 1);
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(boolean) => boolean.to_string(),
        serde_json::Value::Number(number) => js_number_to_string(number),
        serde_json::Value::String(text) => {
            serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
        }
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                return "[]".to_string();
            }
            let rendered = items
                .iter()
                .map(|item| format!("{child_indent}{}", js_stringify_pretty(item, depth + 1)))
                .collect::<Vec<_>>()
                .join(",\n");
            format!("[\n{rendered}\n{indent}]")
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return "{}".to_string();
            }
            let rendered = map
                .iter()
                .map(|(key, item)| {
                    let key = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                    format!(
                        "{child_indent}{key}: {}",
                        js_stringify_pretty(item, depth + 1)
                    )
                })
                .collect::<Vec<_>>()
                .join(",\n");
            format!("{{\n{rendered}\n{indent}}}")
        }
    }
}

/// JS `Number::toString` for JSON numbers (ECMA-262 §7.1.12.1, shortest
/// round-trip digits): integers render bare (`100`, not `100.0`), decimal
/// points stay inside (−6, 21], and scientific notation carries an explicit
/// sign (`1e+21`).
fn js_number_to_string(number: &serde_json::Number) -> String {
    if let Some(unsigned) = number.as_u64() {
        return unsigned.to_string();
    }
    if let Some(signed) = number.as_i64() {
        return signed.to_string();
    }
    let Some(float) = number.as_f64() else {
        return number.to_string();
    };
    if float == float.trunc() && float.abs() < 1e21 {
        // integral value: JS prints it without a fraction part
        if float.abs() < 9.007_199_254_740_992e15 {
            return format!("{}", float as i64);
        }
        // beyond exact i64: render the shortest digits then re-add zeros —
        // Rust's {} for such f64s already yields the integral shortest form
        return format!("{float}");
    }
    // shortest round-trip digits via Rust's LowerExp, re-laid-out per JS rules
    let scientific = format!("{float:e}"); // e.g. "1.25e-7", "3.14159e0"
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let negative = mantissa.starts_with('-');
    let digits = digits.trim_start_matches('0');
    let digit_count = digits.len() as i32;
    // value = 0.digits × 10^n  →  n = exponent + 1
    let n = exponent + 1;

    let body = if digit_count <= n && n <= 21 {
        // whole number with trailing zeros
        format!("{digits}{}", "0".repeat((n - digit_count) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let fraction = if digit_count > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        format!(
            "{}{fraction}e{}{}",
            &digits[..1],
            if e < 0 { '-' } else { '+' },
            e.abs()
        )
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// `renderJsonContent` (format.ts:389-402).
pub fn render_json_content(formatted_json: &str, format: OutputFormat) -> String {
    match format {
        OutputFormat::Json | OutputFormat::Text => formatted_json.to_string(),
        OutputFormat::Html => format!(
            "<pre><code class=\"language-json\">{}</code></pre>",
            escape_html(formatted_json)
        ),
        _ => format!("```json\n{formatted_json}\n```"),
    }
}

/// `stripExtractorComments` (format.ts:404-415). P2 includeReplies support
/// ships the function (byte-parity fixture surface); the pipeline only calls
/// it when includeReplies=false (FR-P1 scope note in requirements §2.3).
static RE_STRIP_HTML: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\s*<hr>\s*<div class="[^"]* comments">.*$"#).expect("valid regex")
});
static RE_STRIP_MD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)\n---\n+## Comments\n.*$").expect("valid regex"));

pub fn strip_extractor_comments(content: &str, format: OutputFormat) -> String {
    if format == OutputFormat::Html {
        return trim_end_js(&RE_STRIP_HTML.replace(content, "")).to_string();
    }
    trim_end_js(&RE_STRIP_MD.replace(content, "")).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_to_fixed_rounds_halves_away_from_zero() {
        // Golden cases generated by node v24 (the JS truth for parity):
        // node -e '[[0.125,2],[1.125,2],[1.25,1],[1.24,1],[2.675,2],[0.375,2],
        // [1024,0],[12.35,1],[1.005,2],[0.5,0],[2.5,0],[12.345,2],
        // [1152/1024,2],[10240/1024,1],[1.5,1],[90/60,1]].map(([v,d]) =>
        // v.toFixed(d))'
        let cases: &[(f64, usize, &str)] = &[
            (0.125, 2, "0.13"),
            (1.125, 2, "1.13"),
            (1.25, 1, "1.3"),
            (1.24, 1, "1.2"),
            (2.675, 2, "2.67"), // double is 2.67499999…; Rust's own format agrees
            (0.375, 2, "0.38"),
            (1024.0, 0, "1024"),
            (12.35, 1, "12.3"),
            (1.005, 2, "1.00"),
            (0.5, 0, "1"), // exact tie: JS rounds up, Rust {:.0} would give "0"
            (2.5, 0, "3"), // exact tie again — the case half-even gets wrong
            (12.345, 2, "12.35"),
            (1152.0 / 1024.0, 2, "1.13"),
            (10240.0 / 1024.0, 1, "10.0"),
            (1.5, 1, "1.5"),
            (90.0 / 60.0, 1, "1.5"),
        ];
        for (value, decimals, expected) in cases {
            assert_eq!(
                &js_to_fixed(*value, *decimals),
                expected,
                "js_to_fixed({value}, {decimals})"
            );
        }
    }

    #[test]
    fn byte_count_formatting_matches_templates() {
        assert_eq!(format_byte_count(0), "0 B");
        assert_eq!(format_byte_count(999), "999 B");
        assert_eq!(format_byte_count(1024), "1.00 KB");
        assert_eq!(format_byte_count(10240), "10.0 KB");
        assert_eq!(format_byte_count(1024 * 1024), "1.00 MB");
    }

    #[test]
    fn duration_formatting_matches_templates() {
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(1500), "1500ms (1.5s)");
        assert_eq!(format_duration_ms(10_000), "10000ms (10s)");
        assert_eq!(format_duration_ms(90_000), "90000ms (1.5m)");
    }

    #[test]
    fn markdown_to_text_strips_markup() {
        let markdown = "# Title\n\n**bold** and *ital* and [link](https://x) ![img](y)\n\n> quote\n- item\n`code`";
        let text = markdown_to_text(markdown);
        // Upstream quirk preserved 1:1: the link regex runs BEFORE the image
        // regex, so `![img](y)` loses its `[img](y)` part first and the
        // leftover `!img` survives (format.ts:242-244 evaluation order).
        assert_eq!(
            text,
            "Title\n\nbold and ital and link !img\n\nquote\n• item\ncode"
        );
    }

    #[test]
    fn truncation_appends_marker() {
        assert_eq!(truncate_content("abc", 5), "abc");
        assert_eq!(truncate_content("abcdef", 3), "abc\n\n[... truncated]");
        // Astral chars count as one char here vs two UTF-16 units upstream —
        // the declared [VARIANT]; fixtures record the drift samples.
        assert_eq!(truncate_content("a😀b😀c", 3), "a😀b\n\n[... truncated]");
    }

    #[test]
    fn word_count_matches_whitespace_split() {
        assert_eq!(estimate_word_count("  a b  c "), 3);
        assert_eq!(estimate_word_count(""), 0);
    }

    #[test]
    fn json_pretty_print_two_space() {
        let formatted = parse_and_format_json("{\"b\":1,\"a\":[1,2]}").expect("parses");
        assert_eq!(
            formatted,
            "{\n  \"b\": 1,\n  \"a\": [\n    1,\n    2\n  ]\n}"
        );
        assert_eq!(
            parse_and_format_json("{invalid"),
            Err("Invalid JSON response".to_string())
        );
    }

    #[test]
    fn error_response_text_shapes() {
        let error = crate::types::FetchError::validation(
            "Invalid URL: foo".to_string(),
            crate::types::FetchErrorCode::InvalidUrl,
            "foo",
        );
        // Non-timeout/http/download errors with retryable=false carry no
        // trailing suggestion line.
        assert_eq!(
            build_fetch_error_response_text(&error),
            "Error: Invalid URL: foo"
        );
    }
}
