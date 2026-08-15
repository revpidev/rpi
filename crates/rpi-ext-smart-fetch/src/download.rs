//! Download filename derivation and sanitization — a pure-function port of
//! the `resolveDownloadTarget` chain in upstream `extract.ts:100-211` @
//! b0111612 — plus the streaming-to-disk path (`streamResponseToFile`
//! extract.ts:228-344 and `buildFileResult` extract.ts:1048-1117) wired in
//! TE07 (FR-P1-4).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::constants::DEFAULT_TEMP_DIR_NAME;
use crate::http::ResponseBody;
use crate::pipeline::{map_fetch_failure, FetchErrorContext, FetchExecutionHooks};
use crate::types::{FetchError, FetchOutcome, FetchResult};

/// `deburr` (lodash): Latin-1/Latin Extended-A diacritics fold to ASCII.
/// `deunicode` folds the whole Unicode range — a superset of lodash deburr;
/// divergence samples land in the byte-parity fixtures (design §3.4).
fn deburr(value: &str) -> String {
    deunicode::deunicode(value).to_string()
}

/// `/^"|"$/g` (extract.ts:143/153): strip at most ONE leading and ONE
/// trailing quote — `trim_matches('"')` eats every quote and would turn
/// `""name""` into `name` where upstream keeps `"name"`.
fn strip_one_quote_pair(value: &str) -> &str {
    let mut result = value;
    if let Some(rest) = result.strip_prefix('"') {
        result = rest;
    }
    if let Some(rest) = result.strip_suffix('"') {
        result = rest;
    }
    result
}

/// `sanitizeBaseName` (extract.ts:100-111).
pub fn sanitize_base_name(value: &str) -> String {
    static RE_SLASHES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\\/]+").expect("valid regex"));
    static RE_INVALID: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._ -]+").expect("valid regex"));
    static RE_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));
    static RE_DASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-+").expect("valid regex"));
    static RE_LEADING_DOTS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\.+").expect("valid regex"));
    static RE_TRAILING: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[. -]+$").expect("valid regex"));

    let value = deburr(value);
    let value = RE_SLASHES.replace_all(&value, "-");
    let value = RE_INVALID.replace_all(&value, "");
    let value = value
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .to_string();
    let value = RE_SPACES.replace_all(&value, "-");
    let value = RE_DASHES.replace_all(&value, "-");
    let value = RE_LEADING_DOTS.replace_all(&value, "");
    RE_TRAILING.replace_all(&value, "").into_owned()
}

/// `sanitizeExtension` (extract.ts:113-121).
pub fn sanitize_extension(value: &str) -> String {
    static RE_LEADING: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[.\s]+").expect("valid regex"));
    static RE_SLASHES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\\/]+").expect("valid regex"));
    static RE_INVALID: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_-]+").expect("valid regex"));

    let value = deburr(value);
    let value = RE_LEADING.replace_all(&value, "");
    let value = RE_SLASHES.replace_all(&value, "");
    let raw: String = RE_INVALID.replace_all(&value, "").to_lowercase();
    if raw.is_empty() {
        String::new()
    } else {
        format!(".{raw}")
    }
}

/// `decodeContentDispositionFilename` (extract.ts:123-129):
/// percent-decode, falling back to the raw value on malformed input.
fn decode_content_disposition_filename(value: &str) -> String {
    percent_decode(value).unwrap_or_else(|| value.to_string())
}

/// Minimal percent-decoder (`decodeURIComponent` subset): malformed escapes
/// return None (JS throws, upstream catches and keeps the raw value).
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let high = (hex[0] as char).to_digit(16)?;
            let low = (hex[1] as char).to_digit(16)?;
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Node `path.parse` subset for the filename split: `name` / `ext` around
/// the LAST dot of the base (dotfiles keep their dot in `name`, `ext` empty).
fn parse_name_ext(base: &str) -> (String, String) {
    let file = base.rsplit(['/', '\\']).next().unwrap_or(base);
    if file.starts_with('.') {
        // path.parse(".bashrc") → { name: ".bashrc", ext: "" }
        return (file.to_string(), String::new());
    }
    match file.rfind('.') {
        Some(position) if position > 0 => {
            (file[..position].to_string(), file[position..].to_string())
        }
        _ => (file.to_string(), String::new()),
    }
}

/// `extractContentDispositionFilename` (extract.ts:131-161). Returns the
/// sanitized base name + extension when a `filename`/`filename*` is present.
pub fn extract_content_disposition_filename(content_disposition: &str) -> DispositionName {
    static RE_FILENAME_STAR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)filename\*=([^;]+)").expect("valid regex"));
    static RE_FILENAME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)filename=(?:"([^"]+)"|([^;]+))"#).expect("valid regex"));

    let raw_filename = match RE_FILENAME_STAR.captures(content_disposition) {
        Some(star) => {
            // RFC 5987: `charset''value` — keep everything after the first ''.
            let value = star[1].trim();
            let encoded = match value.split_once("''") {
                Some((_, rest)) => rest,
                None => value,
            };
            let encoded = strip_one_quote_pair(encoded);
            decode_content_disposition_filename(encoded)
        }
        None => match RE_FILENAME.captures(content_disposition) {
            Some(plain) => plain
                .get(1)
                .or_else(|| plain.get(2))
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default(),
            None => String::new(),
        },
    };

    if raw_filename.is_empty() {
        return DispositionName::default();
    }
    // strip surrounding quotes, then turn path separators into dashes
    let sanitized = strip_one_quote_pair(&raw_filename).to_string();
    let sanitized = sanitized.replace(['/', '\\'], "-");
    let (name, ext) = parse_name_ext(&sanitized);
    let base = if name.is_empty() {
        sanitized.clone()
    } else {
        name
    };
    DispositionName {
        base_name: Some(sanitize_base_name(&base)),
        extension: Some(sanitize_extension(&ext)),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DispositionName {
    pub base_name: Option<String>,
    pub extension: Option<String>,
}

/// `deriveUrlPathName` (extract.ts:163-184).
pub fn derive_url_path_name(url: &str) -> DispositionName {
    let Ok(parsed) = url::Url::parse(url) else {
        return DispositionName::default();
    };
    let last_segment = parsed
        .path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty()));
    let Some(last_segment) = last_segment else {
        return DispositionName::default();
    };
    let decoded = decode_content_disposition_filename(last_segment);
    let (name, ext) = parse_name_ext(&decoded);
    let base = if name.is_empty() {
        decoded.clone()
    } else {
        name
    };
    DispositionName {
        base_name: Some(sanitize_base_name(&base)).filter(|s| !s.is_empty()),
        extension: Some(sanitize_extension(&ext)).filter(|s| !s.is_empty()),
    }
}

/// `resolveExtensionFromMimeType` (extract.ts:186-191): `mime-types`
/// `extension(contentType)`; `mime_guess` shares the mime-db lineage — drift
/// cases patch in the table below (fixtures arbitrate).
pub fn resolve_extension_from_mime_type(content_type: &str) -> String {
    let normalized = crate::pipeline::normalize_content_type(content_type);
    let extension = crate::pipeline::MIME_PATCH_TABLE
        .get(normalized.as_str())
        .cloned()
        .or_else(|| {
            mime_guess::get_mime_extensions_str(&normalized)
                .and_then(|exts| exts.first().map(|s| s.to_string()))
        })
        .unwrap_or_default();
    let sanitized = sanitize_extension(&extension);
    if sanitized.is_empty() {
        ".dat".to_string()
    } else {
        sanitized
    }
}

/// `resolveDownloadTarget` (extract.ts:193-211). `fallback_uuid` replaces the
/// upstream `randomUUID()` call so the function stays pure for fixtures.
pub fn resolve_download_target(
    final_url: &str,
    content_disposition: &str,
    content_type: &str,
    fallback_uuid: &str,
) -> DownloadTarget {
    let from_disposition = extract_content_disposition_filename(content_disposition);
    let from_url = derive_url_path_name(final_url);
    let base_name = from_disposition
        .base_name
        .filter(|s| !s.is_empty())
        .or_else(|| from_url.base_name.filter(|s| !s.is_empty()))
        .unwrap_or_else(|| sanitize_base_name(fallback_uuid));
    let extension = from_disposition
        .extension
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| resolve_extension_from_mime_type(content_type));

    DownloadTarget {
        file_name: format!("{base_name}{extension}"),
        extension,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTarget {
    pub file_name: String,
    pub extension: String,
}

// ===== streaming download path (FR-P1-4, extract.ts:213-344 + 1048-1117) =====

/// A download failure carrying the bytes already streamed — the port of the
/// `errorContext.downloadedBytes` bookkeeping upstream's `onRequestEvent`
/// `body_progress` events provide (the crates.io engine has no event stream,
/// declared deviation TE-D23; counting at the write loop recovers the number).
#[derive(Debug)]
pub struct DownloadFailure {
    pub message: String,
    pub downloaded_bytes: u64,
    /// `true` for the `EEXIST` retry signal (create_new collisions).
    pub already_exists: bool,
}

impl DownloadFailure {
    fn io(error: &std::io::Error, downloaded_bytes: u64) -> Self {
        DownloadFailure {
            message: error.to_string(),
            downloaded_bytes,
            already_exists: error.kind() == ErrorKind::AlreadyExists,
        }
    }
}

/// `cleanupPartialFile` (extract.ts:213-226): best-effort unlink; a missing
/// file is fine (ENOENT), other failures propagate.
fn cleanup_partial_file(file_path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(file_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Open the target for exclusive creation with 0600 (Node's
/// `createWriteStream(path, { flags: "wx", mode: 0o600 })`).
fn create_exclusive(file_path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(file_path)
}

/// `streamResponseToFile` (extract.ts:228-344): stream the body to
/// `file_path` with exclusive creation and 0600, counting bytes; on failure
/// remove the partial file (unless the failure is the EEXIST retry signal)
/// and surface the byte count for the error context. The unconsumed body
/// rides back on error — EEXIST fires at file-open time, before the stream
/// is read, so the retry loop re-streams it (upstream re-`getReader()`s the
/// same untouched stream).
pub async fn stream_response_to_file(
    body: ResponseBody,
    file_path: &Path,
) -> Result<u64, (ResponseBody, DownloadFailure)> {
    stream_response_to_file_with_progress(body, file_path, &FetchExecutionHooks::default(), None)
        .await
}

/// `streamResponseToFile` with the FR-P2-A progress seam: each accepted
/// chunk emits a `body_progress` event at
/// `0.51 + fraction × 0.44` (mapRequestEventToProgress) when the content
/// length is known — without one the upstream mapping stays at 0.51 (no
/// information), so no frame is emitted (TE-D27).
pub async fn stream_response_to_file_with_progress(
    body: ResponseBody,
    file_path: &Path,
    hooks: &FetchExecutionHooks,
    content_length: Option<u64>,
) -> Result<u64, (ResponseBody, DownloadFailure)> {
    // mapRequestEventToProgress `body_progress` (extract.ts:875-895).
    let body_progress = |downloaded: u64| {
        let Some(content_length) = content_length.filter(|len| *len > 0) else {
            return;
        };
        let fraction = (downloaded as f64 / content_length as f64).clamp(0.0, 1.0);
        hooks.emit_progress("loading", 0.51 + fraction * 0.44, "body_progress");
    };

    if let Some(parent) = file_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return Err((body, DownloadFailure::io(&error, 0)));
        }
    }

    let mut downloaded_bytes = 0u64;
    let mut file = match create_exclusive(file_path) {
        Ok(file) => file,
        Err(error) => return Err((body, DownloadFailure::io(&error, 0))),
    };

    use std::io::Write as _;
    let fail = |body: ResponseBody,
                downloaded_bytes: u64,
                error: std::io::Error|
     -> (ResponseBody, DownloadFailure) {
        let failure = DownloadFailure::io(&error, downloaded_bytes);
        if !failure.already_exists {
            let _ = cleanup_partial_file(file_path);
        }
        (body, failure)
    };

    match body {
        ResponseBody::Full(bytes) => {
            if let Err(error) = file.write_all(&bytes) {
                return Err(fail(
                    ResponseBody::Full(Vec::new()),
                    downloaded_bytes,
                    error,
                ));
            }
            // One buffered chunk (mock transports): the single frame the
            // buffered body can produce.
            downloaded_bytes = bytes.len() as u64;
            body_progress(downloaded_bytes);
            if let Err(error) = finalize_permissions(file_path) {
                return Err(fail(
                    ResponseBody::Full(Vec::new()),
                    downloaded_bytes,
                    error,
                ));
            }
            Ok(bytes.len() as u64)
        }
        ResponseBody::Stream(response) => {
            use futures::StreamExt as _;
            let mut stream = response.bytes_stream();
            loop {
                let chunk = match stream.next().await {
                    Some(Ok(chunk)) => chunk,
                    Some(Err(error)) => {
                        let failure = DownloadFailure {
                            message: error.to_string(),
                            downloaded_bytes,
                            already_exists: false,
                        };
                        let _ = cleanup_partial_file(file_path);
                        // A mid-stream failure never retries, so the body
                        // need not be returned intact.
                        return Err((ResponseBody::Full(Vec::new()), failure));
                    }
                    None => break,
                };
                downloaded_bytes += chunk.len() as u64;
                body_progress(downloaded_bytes);
                if let Err(error) = file.write_all(&chunk) {
                    return Err(fail(
                        ResponseBody::Full(Vec::new()),
                        downloaded_bytes,
                        error,
                    ));
                }
            }
            if let Err(error) = file.flush() {
                return Err(fail(
                    ResponseBody::Full(Vec::new()),
                    downloaded_bytes,
                    error,
                ));
            }
            if let Err(error) = finalize_permissions(file_path) {
                return Err(fail(
                    ResponseBody::Full(Vec::new()),
                    downloaded_bytes,
                    error,
                ));
            }
            Ok(downloaded_bytes)
        }
    }
}

/// Post-write `chmod(filePath, 0o600)` (extract.ts:269/308/330): the open-time
/// mode goes through the process umask, the explicit chmod pins it. A failed
/// chmod THROWS upstream (the `await chmod` sits inside the try), failing the
/// download — it is not silently skipped.
fn finalize_permissions(file_path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(file_path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = file_path;
        Ok(())
    }
}

/// `buildFileResult` (extract.ts:1048-1117). Streams the response into the
/// temp dir (EEXIST retries up to 100 times with `<base>-<n>` names) and
/// builds the `kind: "file"` FetchResult; failures map into the FetchError
/// model exactly like upstream's rethrow through `buildThrownFetchError`
/// (message-regex classification at `phase: loading` with a mime type).
#[allow(clippy::too_many_arguments)]
pub async fn build_file_result(
    opts_url: &str,
    mut body: crate::http::ResponseBody,
    final_url: &str,
    content_type: &str,
    content_disposition: &str,
    browser: &str,
    os: &str,
    temp_dir: Option<&str>,
    context: &FetchErrorContext,
    hooks: &FetchExecutionHooks,
    content_length: Option<u64>,
) -> FetchOutcome {
    // extract.ts:1057-1058: `opts.tempDir || join(tmpdir(), "smart-fetch")`.
    // rpi derives the default subdirectory from the product name — the same
    // declared [VARIANT] as the settings layer (requirements §3).
    let temp_dir: PathBuf = match temp_dir {
        Some(dir) => PathBuf::from(dir),
        None => std::env::temp_dir().join(DEFAULT_TEMP_DIR_NAME),
    };
    if let Err(error) = std::fs::create_dir_all(&temp_dir) {
        return download_error_from_io(&error.to_string(), content_type, final_url, context);
    }

    let target = resolve_download_target(
        final_url,
        content_disposition,
        content_type,
        &uuid::Uuid::new_v4().to_string(),
    );
    let mut file_path = temp_dir.join(&target.file_name);
    let mut attempt = 1u32;

    loop {
        if attempt > 100 {
            break;
        }
        match stream_response_to_file_with_progress(body, &file_path, hooks, content_length).await {
            Ok(file_size) => {
                let mut result = FetchResult::content(opts_url, final_url, 0, "", browser, os);
                result.kind = "file".to_string();
                result.site = url::Url::parse(final_url)
                    .map(|parsed| parsed.host_str().unwrap_or_default().to_string())
                    .unwrap_or_default();
                result.file_path = Some(file_path.to_string_lossy().into_owned());
                result.file_size = Some(file_size);
                result.mime_type = Some(crate::pipeline::normalize_content_type(content_type))
                    .filter(|mime| !mime.is_empty());
                return FetchOutcome::Result(result);
            }
            Err((returned_body, failure)) => {
                body = returned_body;
                if failure.already_exists {
                    // extract.ts:1096-1100: `<sanitized original base>-<attempt>`
                    // — `fileName` stays the INITIAL name through the retries.
                    let (name, _) = parse_name_ext(&target.file_name);
                    let next_base = {
                        let sanitized = sanitize_base_name(&name);
                        if sanitized.is_empty() {
                            uuid::Uuid::new_v4().to_string()
                        } else {
                            sanitized
                        }
                    };
                    file_path = temp_dir.join(format!("{next_base}-{attempt}{}", target.extension));
                    attempt += 1;
                    continue;
                }
                return download_error_from(failure, content_type, final_url, context);
            }
        }
    }

    // extract.ts:1107-1116: 100 collisions exhausted.
    let mut error = FetchError {
        error: format!("Unable to create a unique temp file for {final_url}"),
        code: Some(crate::types::FetchErrorCode::DownloadError),
        phase: Some(crate::types::FetchErrorPhase::Loading),
        retryable: Some(true),
        timeout_ms: None,
        url: Some(opts_url.to_string()),
        final_url: Some(final_url.to_string()),
        status_code: None,
        status_text: None,
        mime_type: Some(crate::pipeline::normalize_content_type(content_type))
            .filter(|mime| !mime.is_empty()),
        content_length: None,
        downloaded_bytes: None,
    };
    // extract.ts:1107-1116: the terminal literal carries timeoutMs but NOT
    // contentLength (the never-started write has no length to report).
    error.timeout_ms = Some(context.timeout_ms);
    FetchOutcome::Error(error)
}

/// Map a mid-download failure through the transport classifier — upstream's
/// rethrow lands in the outer `catch` → `buildThrownFetchError(error, ctx)`
/// with `phase: loading` and a mime type set, producing `download_error` for
/// unclassified messages.
fn download_error_from(
    failure: DownloadFailure,
    content_type: &str,
    final_url: &str,
    context: &FetchErrorContext,
) -> FetchOutcome {
    let classified = crate::http::classify_message(&failure.message).unwrap_or(
        crate::http::TransportFailure::Other {
            message: failure.message.clone(),
        },
    );
    let mut context = context.clone();
    context.final_url = Some(final_url.to_string());
    context.phase = crate::types::FetchErrorPhase::Loading;
    context.mime_type =
        Some(crate::pipeline::normalize_content_type(content_type)).filter(|mime| !mime.is_empty());
    context.downloaded_bytes = Some(failure.downloaded_bytes);
    let fetch_failure = crate::http::FetchFailure::Transport {
        failure: classified,
        final_url: Some(final_url.to_string()),
    };
    FetchOutcome::Error(map_fetch_failure(&fetch_failure, &context))
}

fn download_error_from_io(
    message: &str,
    content_type: &str,
    final_url: &str,
    context: &FetchErrorContext,
) -> FetchOutcome {
    download_error_from(
        DownloadFailure {
            message: message.to_string(),
            downloaded_bytes: 0,
            already_exists: false,
        },
        content_type,
        final_url,
        context,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_name_sanitization_chain() {
        // upstream chain: deburr → slashes → invalid → trim → spaces → dashes
        // → leading dots → trailing ". -"
        assert_eq!(sanitize_base_name("Héllo Wörld.pdf"), "Hello-World.pdf");
        assert_eq!(sanitize_base_name("a/b\\c"), "a-b-c");
        assert_eq!(sanitize_base_name("  spaced  out  "), "spaced-out");
        assert_eq!(sanitize_base_name(".hidden"), "hidden");
        assert_eq!(sanitize_base_name("trailing... "), "trailing");
    }

    #[test]
    fn extension_sanitization() {
        assert_eq!(sanitize_extension(".TXT"), ".txt");
        assert_eq!(sanitize_extension("tar.gz"), ".targz");
        assert_eq!(sanitize_extension(""), "");
    }

    #[test]
    fn quote_stripping_removes_one_pair_only() {
        // extract.ts:143/153 `replace(/^"|"$/g, "")` — ONE leading and ONE
        // trailing quote, never the whole run: `""x""` keeps `"x"`. The
        // sanitize chain then dashes the inner quotes away.
        assert_eq!(strip_one_quote_pair("\"name\""), "name");
        assert_eq!(strip_one_quote_pair("\"name"), "name");
        assert_eq!(strip_one_quote_pair("name\""), "name");
        assert_eq!(strip_one_quote_pair("\"\"name\"\""), "\"name\"");
        assert_eq!(strip_one_quote_pair("no quotes"), "no quotes");
        // End to end: double-wrapped filename still resolves (inner quotes
        // are invalid chars, sanitized to nothing by the base-name chain).
        let target = resolve_download_target(
            "https://example.com/x",
            "attachment; filename=\"\"Report.pdf\"\"",
            "application/pdf",
            "00000000-0000-0000-0000-000000000000",
        );
        assert_eq!(target.file_name, "Report.pdf");
    }

    #[test]
    fn disposition_filename_star_wins() {
        // filename* (RFC 5987) decodes "na%C3%AFve%20name.md" → "naïve
        // name.md" → deburr folds ï → i, spaces → dashes.
        let target = resolve_download_target(
            "https://example.com/x",
            "attachment; filename=\"fallback.txt\"; filename*=UTF-8''na%C3%AFve%20name.md",
            "text/plain",
            "00000000-0000-0000-0000-000000000000",
        );
        assert_eq!(target.file_name, "naive-name.md");
    }

    #[test]
    fn disposition_plain_filename() {
        // Quoted and unquoted plain filename=; the base keeps case, the
        // extension lowercases through sanitizeExtension.
        let quoted = resolve_download_target(
            "https://example.com/downloads",
            "attachment; filename=\"My Report.PDF\"",
            "application/pdf",
            "00000000-0000-0000-0000-000000000000",
        );
        assert_eq!(quoted.file_name, "My-Report.pdf");
        let plain = resolve_download_target(
            "https://example.com/dl",
            "attachment; filename=My Report.PDF",
            "application/pdf",
            "00000000-0000-0000-0000-000000000000",
        );
        assert_eq!(plain.file_name, "My-Report.pdf");
    }

    #[test]
    fn url_segment_fallback_and_uuid() {
        let target = resolve_download_target(
            "https://example.com/files/report.pdf",
            "",
            "application/pdf",
            "00000000-0000-0000-0000-000000000000",
        );
        assert_eq!(target.file_name, "report.pdf");

        // Trailing-slash URLs still expose their last non-empty path segment
        // upstream ("/files/".split("/").filter(Boolean).at(-1) === "files").
        let target = resolve_download_target(
            "https://example.com/files/",
            "",
            "application/pdf",
            "00000000-0000-0000-0000-000000000000",
        );
        assert_eq!(target.file_name, "files.pdf");

        // No path segment at all → UUID base name with the .dat fallback.
        let target =
            resolve_download_target("https://example.com/", "", "unknown/unknown", "fixed-uuid");
        assert_eq!(target.file_name, "fixed-uuid.dat");
    }
}
