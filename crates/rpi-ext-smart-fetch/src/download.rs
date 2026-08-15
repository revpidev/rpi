//! Download filename derivation and sanitization — a pure-function port of
//! the `resolveDownloadTarget` chain in upstream `extract.ts:100-211` @
//! b0111612 (FR-P1-4 surface; TE06 ships the functions + fixtures, TE07
//! wires the streaming download path).

use std::sync::LazyLock;

use regex::Regex;

/// `deburr` (lodash): Latin-1/Latin Extended-A diacritics fold to ASCII.
/// `deunicode` folds the whole Unicode range — a superset of lodash deburr;
/// divergence samples land in the byte-parity fixtures (design §3.4).
fn deburr(value: &str) -> String {
    deunicode::deunicode(value).to_string()
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
            let encoded = encoded.trim_matches('"');
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
    let sanitized = raw_filename
        .trim_start_matches('"')
        .trim_end_matches('"')
        .to_string();
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
