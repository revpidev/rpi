pub fn trusted_http_url(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_normalized_http_urls() {
        assert_eq!(
            trusted_http_url("https://example.com/device"),
            Some("https://example.com/device".to_owned())
        );
        assert_eq!(
            trusted_http_url("http://localhost/device"),
            Some("http://localhost/device".to_owned())
        );
        for value in ["file:///etc/passwd", "javascript:alert(1)", "", "not a url"] {
            assert_eq!(trusted_http_url(value), None, "{value}");
        }
    }
}
