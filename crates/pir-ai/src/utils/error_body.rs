//! Port of `packages/ai/src/utils/error-body.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: the upstream SDK-error shape probing (Mistral
//! `statusCode`, `openai` SDK `error`, AWS `$metadata`/`$response`) inspects
//! TS exception objects; pir adapters use reqwest directly, so status/body are
//! extracted from HTTP responses at the call site and passed in via
//! [`NormalizedProviderError`]. The display-string composition
//! ([`format_provider_error`], [`truncate_error_text`], the 4000-char cap) is
//! ported byte-for-byte.

pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

/// `NormalizedProviderError`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NormalizedProviderError {
    /// HTTP status code, when one could be extracted.
    pub status: Option<u16>,
    /// Raw HTTP body reason, already trimmed and truncated to the cap.
    pub body: Option<String>,
    /// The base error message.
    pub message: String,
    /// True when `message` already contains the body (no separate body to add).
    pub message_carries_body: bool,
}

impl NormalizedProviderError {
    /// Builds a normalized error from its parts, deriving
    /// `message_carries_body` the same way upstream does
    /// (`body === undefined || message.includes(body)`).
    pub fn new(status: Option<u16>, body: Option<String>, message: String) -> Self {
        let body = body.and_then(|body| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(truncate_error_text(trimmed, MAX_PROVIDER_ERROR_BODY_CHARS))
            }
        });
        let message_carries_body = match &body {
            None => true,
            Some(body) => message.contains(body.as_str()),
        };
        Self {
            status,
            body,
            message,
            message_carries_body,
        }
    }
}

/// `formatProviderError`: composes a display string from a normalized error.
///
/// - message already carries the body, or body/status missing: message
///   unchanged (with `<prefix> (<status>): ` prefix when both are present)
/// - otherwise: `"<status>: <body>"` or `"<prefix> (<status>): <body>"`
pub fn format_provider_error(norm: &NormalizedProviderError, prefix: Option<&str>) -> String {
    if norm.message_carries_body || norm.status.is_none() || norm.body.is_none() {
        return match (prefix, norm.status) {
            (Some(prefix), Some(status)) => format!("{prefix} ({status}): {}", norm.message),
            _ => norm.message.clone(),
        };
    }
    let (status, body) = (norm.status.unwrap(), norm.body.as_ref().unwrap());
    // invariant: is_none() cases returned above
    match prefix {
        Some(prefix) => format!("{prefix} ({status}): {body}"),
        None => format!("{status}: {body}"),
    }
}

/// `truncateErrorText`: caps text at `max_chars`, appending a marker with the
/// dropped character count. JS counts UTF-16 code units; this counts `char`s
/// (BMP-equivalent, see D-003).
pub fn truncate_error_text(text: &str, max_chars: usize) -> String {
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}... [truncated {} chars]", len - max_chars)
}

/// `safeJsonStringify`.
pub fn safe_json_stringify<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_error_text() {
        assert_eq!(truncate_error_text("abc", 4), "abc");
        assert_eq!(
            truncate_error_text("abcde", 4),
            "abcd... [truncated 1 chars]"
        );
    }

    #[test]
    fn test_format_provider_error_message_carries_body() {
        let norm = NormalizedProviderError::new(
            Some(403),
            Some("body text".to_owned()),
            "403: body text".to_owned(),
        );
        assert!(norm.message_carries_body);
        assert_eq!(format_provider_error(&norm, None), "403: body text");
        assert_eq!(
            format_provider_error(&norm, Some("anthropic")),
            "anthropic (403): 403: body text"
        );
    }

    #[test]
    fn test_format_provider_error_separate_body() {
        let norm = NormalizedProviderError::new(
            Some(429),
            Some("rate limited".to_owned()),
            "status code (no body)".to_owned(),
        );
        assert!(!norm.message_carries_body);
        assert_eq!(format_provider_error(&norm, None), "429: rate limited");
        assert_eq!(
            format_provider_error(&norm, Some("openai")),
            "openai (429): rate limited"
        );
    }

    #[test]
    fn test_format_provider_error_no_status_or_body() {
        let norm = NormalizedProviderError::new(None, None, "boom".to_owned());
        assert_eq!(format_provider_error(&norm, Some("x")), "boom");
        // No body: message unchanged, but a present status still prefixes.
        let no_body = NormalizedProviderError::new(Some(500), None, "boom".to_owned());
        assert_eq!(format_provider_error(&no_body, Some("x")), "x (500): boom");
        assert_eq!(format_provider_error(&no_body, None), "boom");
    }
}
