use std::sync::Arc;

use serde_json::{Map, Value};
use url::Url;

pub type MetadataDocumentCallbackCheck = Arc<dyn Fn(Option<&str>) -> Option<String> + Send + Sync>;

#[derive(Clone, Default)]
pub struct McpServerConfigOptions {
    pub check_metadata_document_callback: Option<MetadataDocumentCallbackCheck>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpServerConfigResult {
    Stdio(Value),
    Http(Value),
    Invalid(String),
}

pub fn is_mcp_server_override(entry: &Map<String, Value>) -> bool {
    !entry.contains_key("command") && !entry.contains_key("url") && !entry.contains_key("type")
}

pub fn validate_mcp_server_config(
    name: &str,
    raw: &Value,
    options: &McpServerConfigOptions,
) -> McpServerConfigResult {
    let invalid = |error: String| McpServerConfigResult::Invalid(error);
    if name.is_empty()
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        return invalid(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let Some(entry) = raw.as_object() else {
        return invalid(format!("server \"{name}\" must be an object"));
    };
    if let Some(error) = validate_base_fields(entry) {
        return invalid(format!("server \"{name}\": {error}"));
    }
    let kind = entry.get("type").and_then(Value::as_str);
    if kind == Some("sse") {
        return invalid(format!(
            "server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
        ));
    }
    if entry.get("url").is_some_and(Value::is_string)
        && matches!(kind, None | Some("http") | Some("streamable-http"))
    {
        let url = entry.get("url").and_then(Value::as_str).unwrap_or_default();
        if let Err(error) = validate_http_fields(entry, url, options) {
            return invalid(format!("server \"{name}\": {error}"));
        }
        return McpServerConfigResult::Http(raw.clone());
    }
    if entry.get("command").is_some_and(Value::is_string) && matches!(kind, None | Some("stdio")) {
        if let Err(error) = validate_stdio_fields(entry) {
            return invalid(format!("server \"{name}\": {error}"));
        }
        return McpServerConfigResult::Stdio(raw.clone());
    }
    invalid(format!(
        "server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
    ))
}

fn validate_base_fields(entry: &Map<String, Value>) -> Option<String> {
    if entry
        .get("enabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Some("enabled must be a boolean".to_owned());
    }
    if entry
        .get("description")
        .is_some_and(|value| !value.is_string())
    {
        return Some("description must be a string".to_owned());
    }
    if entry.get("timeout").is_some_and(|value| {
        value
            .as_f64()
            .is_none_or(|timeout| !timeout.is_finite() || timeout <= 0.0)
    }) {
        return Some("timeout must be a positive number of seconds".to_owned());
    }
    None
}

fn validate_http_fields(
    entry: &Map<String, Value>,
    url: &str,
    options: &McpServerConfigOptions,
) -> Result<(), String> {
    let valid = Url::parse(url)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"));
    if !valid {
        return Err("url must be an http or https URL".to_owned());
    }
    if entry
        .get("headers")
        .is_some_and(|value| !is_string_record(value))
    {
        return Err("headers must map names to strings".to_owned());
    }
    validate_oauth(entry.get("oauth"), options)
}

fn validate_stdio_fields(entry: &Map<String, Value>) -> Result<(), String> {
    if entry.get("args").is_some_and(|value| {
        value
            .as_array()
            .is_none_or(|args| !args.iter().all(Value::is_string))
    }) {
        return Err("args must be an array of strings".to_owned());
    }
    if entry
        .get("env")
        .is_some_and(|value| !is_string_record(value))
    {
        return Err("env must map names to strings".to_owned());
    }
    if entry.get("cwd").is_some_and(|value| !value.is_string()) {
        return Err("cwd must be a string".to_owned());
    }
    Ok(())
}

fn is_string_record(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.values().all(Value::is_string))
}

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

fn is_loopback_host(host: &str) -> bool {
    let normalized = host.trim_matches(['[', ']']);
    LOOPBACK_HOSTS
        .iter()
        .any(|candidate| candidate.trim_matches(['[', ']']) == normalized)
}

pub fn is_loopback_redirect_uri(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    url.scheme() == "http"
        && is_loopback_host(url.host_str().unwrap_or_default())
        && url.query().is_none()
        && url.fragment().is_none()
}

fn validate_oauth(value: Option<&Value>, options: &McpServerConfigOptions) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(map) = value.as_object() else {
        return Err("oauth must be an object".to_owned());
    };
    if map.get("clientId").is_some_and(|value| !value.is_string()) {
        return Err("oauth.clientId must be a string".to_owned());
    }
    if map
        .get("clientSecret")
        .is_some_and(|value| !value.is_string())
    {
        return Err("oauth.clientSecret must be a string".to_owned());
    }
    let port = map.get("callbackPort");
    if let Some(port) = port {
        let valid = port
            .as_u64()
            .is_some_and(|port| (1..=65535).contains(&port));
        if !valid {
            return Err("oauth.callbackPort must be a port number".to_owned());
        }
    }
    if let Some(callback_url) = map.get("callbackUrl") {
        let Some(callback_url) = callback_url.as_str() else {
            return Err("oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment".to_owned());
        };
        if !is_loopback_redirect_uri(callback_url) {
            return Err("oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment".to_owned());
        }
        let url_port = Url::parse(callback_url).ok().and_then(|url| url.port());
        if let (Some(url_port), Some(port)) = (url_port, port.and_then(Value::as_u64))
            && url_port as u64 != port
        {
            return Err("oauth.callbackUrl and oauth.callbackPort name different ports".to_owned());
        }
    }
    if map.get("scope").is_some_and(|value| !value.is_string()) {
        return Err("oauth.scope must be a string".to_owned());
    }
    if let Some(client_name) = map.get("clientName") {
        let valid = client_name
            .as_str()
            .is_some_and(|name| !name.trim().is_empty());
        if !valid {
            return Err("oauth.clientName must be a non-empty string".to_owned());
        }
    }
    if let Some(registration) = map.get("clientRegistration")
        && registration.as_str() != Some("auto")
        && registration.as_str() != Some("dcr")
    {
        if registration.as_str() != Some("cimd") {
            return Err(
                "oauth.clientRegistration must be \"auto\", \"dcr\", or \"cimd\"".to_owned(),
            );
        }
        if map.contains_key("clientId") || map.contains_key("clientName") {
            return Err(
                "oauth.clientRegistration \"cimd\" cannot be combined with oauth.clientId or oauth.clientName"
                    .to_owned(),
            );
        }
        let Some(check) = &options.check_metadata_document_callback else {
            return Err(
                "oauth.clientRegistration \"cimd\" is not supported: there are no Client ID Metadata Documents"
                    .to_owned(),
            );
        };
        if let Some(error) = check(
            map.get("callbackUrl")
                .and_then(Value::as_str)
                .map(|url| url as &str),
        ) {
            return Err(error);
        }
    }
    if let Some(metadata_url) = map.get("authServerMetadataUrl") {
        let url = metadata_url
            .as_str()
            .and_then(|value| Url::parse(value).ok());
        let valid = url.as_ref().is_some_and(|url| {
            url.scheme() == "https"
                || (url.scheme() == "http" && is_loopback_host(url.host_str().unwrap_or_default()))
        });
        if !valid {
            return Err(
                "oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tags_transports_and_preserves_application_fields() {
        let raw = json!({
            "url": "https://example.com/mcp",
            "oauth": {"clientRegistration": "auto"},
            "auth": {"provider": "custom"},
            "exposure": "deferred"
        });
        let McpServerConfigResult::Http(config) =
            validate_mcp_server_config("docs", &raw, &McpServerConfigOptions::default())
        else {
            panic!("http config")
        };
        assert_eq!(config, raw);
        let McpServerConfigResult::Stdio(_) = validate_mcp_server_config(
            "local",
            &json!({"command": "server", "args": ["--stdio"]}),
            &McpServerConfigOptions::default(),
        ) else {
            panic!("stdio config")
        };
    }

    #[test]
    fn validates_cimd_through_the_application_callback() {
        let raw = json!({
            "url": "https://example.com/mcp",
            "oauth": {"clientRegistration": "cimd", "callbackUrl": "http://localhost/callback"}
        });
        let options = McpServerConfigOptions {
            check_metadata_document_callback: Some(Arc::new(|url| {
                (url.and_then(|url| Url::parse(url).ok())
                    .map(|url| url.path().to_owned())
                    != Some("/callback".to_owned()))
                .then(|| "callback must use /callback".to_owned())
            })),
        };
        assert!(matches!(
            validate_mcp_server_config("docs", &raw, &options),
            McpServerConfigResult::Http(_)
        ));
        let invalid = json!({
            "url": "https://example.com/mcp",
            "oauth": {"clientRegistration": "cimd", "callbackUrl": "http://localhost/other"}
        });
        assert!(matches!(
            validate_mcp_server_config("docs", &invalid, &options),
            McpServerConfigResult::Invalid(_)
        ));
    }

    #[test]
    fn rejects_mismatched_callback_ports_and_bad_transport() {
        let raw = json!({
            "url": "https://example.com/mcp",
            "oauth": {"callbackUrl": "http://localhost:8080/callback", "callbackPort": 8081}
        });
        assert!(matches!(
            validate_mcp_server_config("docs", &raw, &McpServerConfigOptions::default()),
            McpServerConfigResult::Invalid(_)
        ));
        assert!(matches!(
            validate_mcp_server_config(
                "docs",
                &json!({"type": "sse", "url": "https://example.com"}),
                &McpServerConfigOptions::default()
            ),
            McpServerConfigResult::Invalid(_)
        ));
    }

    #[test]
    fn override_has_no_transport_fields() {
        assert!(is_mcp_server_override(&Map::from_iter([(
            "enabled".to_owned(),
            Value::Bool(false)
        )])));
        assert!(!is_mcp_server_override(&Map::from_iter([(
            "command".to_owned(),
            Value::String("server".to_owned())
        )])));
    }
}
