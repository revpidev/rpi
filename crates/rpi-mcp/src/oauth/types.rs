//! OAuth wire types and their structural validation (port of
//! `packages/mcp/src/oauth/types.ts` @ a13d35a74).
//!
//! `null` and `""` optional fields count as absent (v1.0.0 `8ce69e9d2`):
//! servers send them for values they do not have, like `scope: ""`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::errors::OAuthFlowError;

/// `OAuthProtectedResourceMetadata` (types.ts:10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthProtectedResourceMetadata {
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `AuthorizationServerMetadata` (types.ts:17).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    pub response_types_supported: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_metadata_document_supported: Option<bool>,
    /// Whether authorization responses carry an `iss` parameter (RFC 9207).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_response_iss_parameter_supported: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `OAuthTokens` (types.ts:43).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// `OAuthClientMetadata` (types.ts:51).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthClientMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirect_uris: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contacts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tos_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_statement: Option<String>,
}

/// `OAuthClientInformation` + full variant (types.ts:76).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthClientInformation {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<f64>,
    /// Present for registered clients ("full" variant); drives redirect-URI reuse.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirect_uris: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `OAuthDiscoveryState` (types.ts:88).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthDiscoveryState {
    pub authorization_server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata_url: Option<String>,
}

/// `OAuthServerInfo` (types.ts:96).
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthServerInfo {
    pub authorization_server_url: String,
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
}

/// `OAuthChallenge` (types.ts:102).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OAuthChallenge {
    pub resource_metadata_url: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

fn invalid(message: impl Into<String>) -> OAuthFlowError {
    OAuthFlowError::Invalid(message.into())
}

fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, OAuthFlowError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("Invalid {name}")))
}

fn required_string(value: Option<&Value>, name: &str) -> Result<String, OAuthFlowError> {
    match value.and_then(Value::as_str) {
        Some(text) if !text.is_empty() => Ok(text.to_owned()),
        _ => Err(invalid(format!("Invalid {name}"))),
    }
}

/// `absent` (types.ts:106): `null` and `""` count as absent.
fn absent(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.is_empty(),
        _ => false,
    }
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<String>, OAuthFlowError> {
    if absent(value) {
        return Ok(None);
    }
    Ok(Some(required_string(value, name)?))
}

fn optional_strings(
    value: Option<&Value>,
    name: &str,
) -> Result<Option<Vec<String>>, OAuthFlowError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut output = Vec::with_capacity(items.len());
            for item in items {
                let Some(text) = item.as_str() else {
                    return Err(invalid(format!("Invalid {name}")));
                };
                output.push(text.to_owned());
            }
            Ok(Some(output))
        }
        Some(_) => Err(invalid(format!("Invalid {name}"))),
    }
}

/// `safeUrl` (types.ts:121): parses and rejects `javascript:`/`data:`/
/// `vbscript:`; parsing failures are `Invalid` (not network) so discovery
/// falls back to the server origin.
fn safe_url(value: Option<&Value>, name: &str) -> Result<String, OAuthFlowError> {
    let text = required_string(value, name)?;
    let url = url::Url::parse(&text).map_err(|_| invalid(format!("Invalid {name}")))?;
    if matches!(url.scheme(), "javascript" | "data" | "vbscript") {
        return Err(invalid(format!("Invalid {name}")));
    }
    Ok(text)
}

fn optional_url(value: Option<&Value>, name: &str) -> Result<Option<String>, OAuthFlowError> {
    if absent(value) {
        return Ok(None);
    }
    Ok(Some(safe_url(value, name)?))
}

/// `parseProtectedResourceMetadata` (types.ts:141).
pub fn parse_protected_resource_metadata(
    value: &Value,
) -> Result<OAuthProtectedResourceMetadata, OAuthFlowError> {
    let input = object(value, "OAuth protected resource metadata")?;
    let mut extra = input.clone();
    for key in ["resource", "authorization_servers", "scopes_supported"] {
        extra.remove(key);
    }
    Ok(OAuthProtectedResourceMetadata {
        resource: safe_url(
            input.get("resource"),
            "OAuth protected resource metadata resource",
        )?,
        authorization_servers: optional_strings(
            input.get("authorization_servers"),
            "authorization_servers",
        )?
        .map(|urls| {
            urls.into_iter()
                .map(|url| safe_url(Some(&Value::String(url)), "authorization server URL"))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?,
        scopes_supported: optional_strings(input.get("scopes_supported"), "scopes_supported")?,
        extra,
    })
}

/// `parseAuthorizationServerMetadata` (types.ts:153).
pub fn parse_authorization_server_metadata(
    value: &Value,
) -> Result<AuthorizationServerMetadata, OAuthFlowError> {
    let input = object(value, "authorization server metadata")?;
    let response_types = optional_strings(
        input.get("response_types_supported"),
        "response_types_supported",
    )?
    .ok_or_else(|| invalid("Invalid response_types_supported"))?;
    let mut extra = input.clone();
    for key in [
        "issuer",
        "authorization_endpoint",
        "token_endpoint",
        "registration_endpoint",
        "scopes_supported",
        "response_types_supported",
        "grant_types_supported",
        "token_endpoint_auth_methods_supported",
        "code_challenge_methods_supported",
        "client_id_metadata_document_supported",
        "authorization_response_iss_parameter_supported",
    ] {
        extra.remove(key);
    }
    Ok(AuthorizationServerMetadata {
        issuer: safe_url(input.get("issuer"), "authorization server issuer")?,
        authorization_endpoint: safe_url(
            input.get("authorization_endpoint"),
            "authorization endpoint",
        )?,
        token_endpoint: safe_url(input.get("token_endpoint"), "token endpoint")?,
        registration_endpoint: optional_url(
            input.get("registration_endpoint"),
            "registration endpoint",
        )?,
        scopes_supported: optional_strings(input.get("scopes_supported"), "scopes_supported")?,
        response_types_supported: response_types,
        grant_types_supported: optional_strings(
            input.get("grant_types_supported"),
            "grant_types_supported",
        )?,
        token_endpoint_auth_methods_supported: optional_strings(
            input.get("token_endpoint_auth_methods_supported"),
            "token_endpoint_auth_methods_supported",
        )?,
        code_challenge_methods_supported: optional_strings(
            input.get("code_challenge_methods_supported"),
            "code_challenge_methods_supported",
        )?,
        client_id_metadata_document_supported: input
            .get("client_id_metadata_document_supported")
            .and_then(Value::as_bool),
        authorization_response_iss_parameter_supported: input
            .get("authorization_response_iss_parameter_supported")
            .and_then(Value::as_bool),
        extra,
    })
}

/// `parseOAuthTokens` (types.ts:178).
pub fn parse_oauth_tokens(value: &Value) -> Result<OAuthTokens, OAuthFlowError> {
    let input = object(value, "OAuth token response")?;
    let expires = if absent(input.get("expires_in")) {
        None
    } else {
        let number = input
            .get("expires_in")
            .and_then(Value::as_f64)
            .ok_or_else(|| invalid("Invalid expires_in"))?;
        if !number.is_finite() {
            return Err(invalid("Invalid expires_in"));
        }
        Some(number)
    };
    Ok(OAuthTokens {
        access_token: required_string(input.get("access_token"), "access_token")?,
        token_type: required_string(input.get("token_type"), "token_type")?,
        expires_in: expires,
        scope: optional_string(input.get("scope"), "scope")?,
        refresh_token: optional_string(input.get("refresh_token"), "refresh_token")?,
        id_token: optional_string(input.get("id_token"), "id_token")?,
    })
}

/// `parseClientInformation` (types.ts:193).
pub fn parse_client_information(value: &Value) -> Result<OAuthClientInformation, OAuthFlowError> {
    let input = object(value, "OAuth client registration response")?;
    let mut extra = input.clone();
    for key in [
        "client_id",
        "client_secret",
        "client_id_issued_at",
        "client_secret_expires_at",
        "redirect_uris",
    ] {
        extra.remove(key);
    }
    Ok(OAuthClientInformation {
        client_id: required_string(input.get("client_id"), "client_id")?,
        client_secret: optional_string(input.get("client_secret"), "client_secret")?,
        client_id_issued_at: input.get("client_id_issued_at").and_then(Value::as_f64),
        client_secret_expires_at: input
            .get("client_secret_expires_at")
            .and_then(Value::as_f64),
        redirect_uris: optional_strings(input.get("redirect_uris"), "redirect_uris")?
            .unwrap_or_default(),
        extra,
    })
}

/// `parseWwwAuthenticate` (discovery.ts:47). Only `bearer` and `dpop`
/// challenges are considered; an empty parameter value counts as absent.
pub fn parse_www_authenticate(header: Option<&str>) -> OAuthChallenge {
    let Some(header) = header else {
        return OAuthChallenge::default();
    };
    let scheme = header.split_whitespace().next().unwrap_or_default();
    if !scheme.eq_ignore_ascii_case("bearer") && !scheme.eq_ignore_ascii_case("dpop") {
        return OAuthChallenge::default();
    }
    let resource_metadata = challenge_field(header, "resource_metadata");
    let resource_metadata_url = resource_metadata
        .clone()
        .filter(|value| url::Url::parse(value).is_ok());
    OAuthChallenge {
        resource_metadata_url,
        scope: challenge_field(header, "scope"),
        error: challenge_field(header, "error"),
        error_description: challenge_field(header, "error_description"),
    }
}

/// `field` (discovery.ts:33): `(?:^|[,\s])name=(?:"([^"]*)"|([^\s,]+))`,
/// case-insensitive, first non-empty value.
fn challenge_field(header: &str, name: &str) -> Option<String> {
    let bytes = header.as_bytes();
    let needle = name.as_bytes();
    let mut from = 0;
    while let Some(offset) = find_ignore_ascii_case(bytes, needle, from) {
        from = offset + needle.len();
        let before_ok = offset == 0 || matches!(bytes[offset - 1], b',' | b' ' | b'\t');
        if !before_ok || bytes.get(from) != Some(&b'=') {
            continue;
        }
        let rest = &header[from + 1..];
        if let Some(quoted) = rest.strip_prefix('"') {
            if let Some(end) = quoted.find('"') {
                let value = &quoted[..end];
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        } else {
            let value = rest.split([',', ' ', '\t']).next().unwrap_or_default();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn find_ignore_ascii_case(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() || from > haystack.len() - needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&index| haystack[index..index + needle.len()].eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn null_and_empty_optional_fields_count_as_absent() {
        let tokens = parse_oauth_tokens(&json!({
            "access_token": "a",
            "token_type": "bearer",
            "expires_in": null,
            "scope": "",
            "refresh_token": null,
            "id_token": "",
        }))
        .unwrap();
        assert_eq!(tokens.expires_in, None);
        assert_eq!(tokens.scope, None);
        assert_eq!(tokens.refresh_token, None);
        assert_eq!(tokens.id_token, None);

        let client = parse_client_information(&json!({
            "client_id": "c",
            "client_secret": "",
            "client_id_issued_at": 1.0,
        }))
        .unwrap();
        assert_eq!(client.client_secret, None);
        assert_eq!(client.redirect_uris, Vec::<String>::new());
    }

    #[test]
    fn invalid_resource_url_is_invalid_not_network() {
        let error =
            parse_protected_resource_metadata(&json!({"resource": "not a url"})).unwrap_err();
        assert!(!error.is_network());
        assert!(matches!(error, OAuthFlowError::Invalid(_)));
        let error = parse_protected_resource_metadata(&json!({"resource": "javascript:alert(1)"}))
            .unwrap_err();
        assert!(matches!(error, OAuthFlowError::Invalid(_)));
    }

    #[test]
    fn www_authenticate_parses_bearer_challenges() {
        let challenge = parse_www_authenticate(Some(
            "Bearer resource_metadata=\"https://x/.well-known/oauth-protected-resource\", scope=\"a b\", error=\"insufficient_scope\"",
        ));
        assert_eq!(
            challenge.resource_metadata_url.as_deref(),
            Some("https://x/.well-known/oauth-protected-resource")
        );
        assert_eq!(challenge.scope.as_deref(), Some("a b"));
        assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
        // A different scheme or empty values yield no challenge.
        assert_eq!(
            parse_www_authenticate(Some("Basic realm=x")),
            OAuthChallenge::default()
        );
        let empty = parse_www_authenticate(Some("Bearer scope=\"\""));
        assert_eq!(empty.scope, None);
    }

    #[test]
    fn server_metadata_requires_endpoints() {
        let metadata = parse_authorization_server_metadata(&json!({
            "issuer": "https://as.example",
            "authorization_endpoint": "https://as.example/authorize",
            "token_endpoint": "https://as.example/token",
            "response_types_supported": ["code"],
        }))
        .unwrap();
        assert_eq!(metadata.issuer, "https://as.example");
        assert!(
            parse_authorization_server_metadata(&json!({
                "issuer": "https://as.example",
                "authorization_endpoint": "https://as.example/authorize",
                "token_endpoint": "https://as.example/token",
            }))
            .is_err()
        );
    }
}
