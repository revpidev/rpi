//! OAuth 2.1: authorization code + PKCE, dynamic client registration,
//! `client_credentials`, localhost callback server, automatic and manual
//! flows (FR-P1-04, design §3.7).
//!
//! Counterpart of upstream `mcp-auth-flow.ts` / `mcp-oauth-provider.ts` /
//! `mcp-callback-server.ts` @ 3d953f90.
//!
//! P1-wave scope: the authorization-code+PKCE flow and the
//! `client_credentials` flow are implemented, and the auto-auth call chain
//! (`proxy::attempt_auto_auth`, TE-D09) consumes `authenticate` from the
//! proxy/direct executors. The localhost callback server uses
//! `tokio::net::TcpListener` (design §3.7: no axum).
//!
//! **Security**: tokens MUST NEVER reach tracing logs (G4 red line). The
//! `authenticate` function resolves tokens via the store only; error
//! messages never embed token values.

pub mod store;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::AdapterError;
use crate::metadata::ServerEntry;
use store::{AuthStorageOptions, OAuthCredentialStore, StoredTokens};

/// `MODERN_PROTOCOL_VERSION` (mcp-probe.ts:2).
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// `LEGACY_PROTOCOL_VERSION` (mcp-probe.ts:3).
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-06-18";

/// `DEFAULT_OAUTH_CALLBACK_PORT` — 0 means OS-assigned (mcp-oauth-provider.ts).
pub const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 0;

/// `DEFAULT_OAUTH_CALLBACK_PATH` (mcp-oauth-provider.ts).
pub const DEFAULT_OAUTH_CALLBACK_PATH: &str = "/callback";

/// `CALLBACK_TIMEOUT_MS` (mcp-callback-server.ts:188).
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// PKCE code challenge method (always S256 per RFC 7636 / MCP spec).
const PKCE_CHALLENGE_METHOD: &str = "S256";

/// `AuthStatus` (mcp-auth-flow.ts:44).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    Authenticated,
    Expired,
    NotAuthenticated,
}

/// `OAuthCallbackResult` (mcp-callback-server.ts:166-170).
#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: String,
    pub iss: Option<String>,
}

/// Callback type for `on_authorization_url` (authorization URL notification).
type AuthorizationUrlCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// `AuthenticateOptions` (mcp-auth-flow.ts:50-60).
#[derive(Default)]
pub struct AuthenticateOptions {
    pub on_authorization_url: Option<AuthorizationUrlCallback>,
    pub auth_storage_options: AuthStorageOptions,
    pub signal: Option<tokio_util::sync::CancellationToken>,
    pub skip_issuer_metadata_validation: bool,
}

/// Generate a random PKCE code verifier (43-128 chars, RFC 7636 §4.1).
///
/// Fails closed: 32 random bytes from the OS CSPRNG → base64url (no
/// padding) → 43 chars (min allowed). A /dev/urandom failure surfaces as
/// an error instead of degrading to a predictable fallback — PKCE and the
/// CSRF state are core OAuth defenses (no time-seeded LCG fallback).
fn generate_code_verifier() -> Result<String, AdapterError> {
    let mut bytes = [0u8; 32];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| {
            AdapterError::InvalidConfigValue(format!(
                "CSPRNG unavailable for PKCE verifier (/dev/urandom): {e}"
            ))
        })?;
    if bytes == [0u8; 32] {
        return Err(AdapterError::InvalidConfigValue(
            "CSPRNG returned all-zero bytes for PKCE verifier".to_string(),
        ));
    }
    Ok(base64_url_encode(&bytes))
}

/// Base64url encoding without padding (RFC 4648 §5, RFC 7636).
fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE code challenge: S256 = base64url(sha256(verifier)) (RFC 7636 §4.2).
fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64_url_encode(&digest)
}

/// Generate a random state parameter for CSRF protection.
fn generate_state() -> Result<String, AdapterError> {
    let verifier = generate_code_verifier()?;
    // Use the first 32 chars as state (sufficient entropy for CSRF).
    Ok(verifier[..32.min(verifier.len())].to_string())
}

/// Protected Resource Metadata (RFC 9728) / Authorization Server Metadata
/// (RFC 8414) response. We only extract the fields we need.
#[derive(Debug, Deserialize)]
struct AuthServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    // DCR support
    #[serde(default)]
    #[allow(dead_code)]
    require_pushed_authorization_requests: Option<bool>,
    // RFC 9207: when true the AS sends and requires validating the `iss`
    // callback parameter (mcp-auth-flow.ts:675-676).
    #[serde(default)]
    authorization_response_iss_parameter_supported: Option<bool>,
}

/// Discover the authorization server metadata (RFC 8414). Upstream uses
/// the SDK's `auth()` function which internally does the `.well-known`
/// fetch; we replicate the relevant fetch here.
/// `loadConfiguredDiscoveryState` (#458, mcp-oauth-provider.ts:187-231 @
/// 10a45367): with `oauth.authServerMetadataUrl` configured, metadata is
/// fetched from THAT url (absolute https, validated at config parse) and
/// the issuer is checked against the metadata url's inferred issuer (or
/// its origin when the path carries no well-known marker).
async fn discover_auth_server_metadata_with_override(
    server_url: &str,
    skip_validation: bool,
    override_url: Option<&str>,
) -> Result<AuthServerMetadata, AdapterError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AdapterError::InvalidConfigValue(format!("HTTP client: {e}")))?;

    let (metadata_url, metadata_origin): (String, Option<String>) = match override_url {
        Some(configured) => {
            let parsed = url::Url::parse(configured).map_err(|e| {
                AdapterError::InvalidConfigValue(format!("invalid authServerMetadataUrl: {e}"))
            })?;
            let origin = parsed.origin().ascii_serialization();
            (configured.to_string(), Some(origin))
        }
        None => {
            let parsed = url::Url::parse(server_url).map_err(|e| {
                AdapterError::InvalidConfigValue(format!("invalid server URL: {e}"))
            })?;
            let port_suffix = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
            let origin = format!(
                "{}://{}{}",
                parsed.scheme(),
                parsed.host_str().unwrap_or("localhost"),
                port_suffix
            );
            (
                format!("{origin}/.well-known/oauth-authorization-server"),
                Some(origin),
            )
        }
    };

    let response = client
        .get(&metadata_url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            AdapterError::InvalidConfigValue(format!("auth server metadata fetch: {e}"))
        })?;

    if !response.status().is_success() {
        if override_url.is_some() {
            return Err(AdapterError::InvalidConfigValue(format!(
                "OAuth authServerMetadataUrl request failed with HTTP {}",
                response.status()
            )));
        }
        return Err(AdapterError::InvalidConfigValue(format!(
            "auth server metadata returned {}",
            response.status()
        )));
    }

    let metadata: AuthServerMetadata = response.json().await.map_err(|e| {
        AdapterError::InvalidConfigValue(format!("auth server metadata parse: {e}"))
    })?;

    if override_url.is_some() {
        // `validateConfiguredIssuer` (mcp-oauth-provider.ts:187-210):
        // issuer must be an absolute http(s) URL and must match the
        // metadata url's inferred issuer (well-known path markers) or,
        // failing that, its origin.
        let issuer = metadata.issuer.clone();
        let parsed_issuer = url::Url::parse(&issuer).map_err(|_| {
            AdapterError::InvalidConfigValue(
                "OAuth authorization-server metadata issuer must be an absolute URL".to_string(),
            )
        })?;
        if parsed_issuer.scheme() != "http" && parsed_issuer.scheme() != "https" {
            return Err(AdapterError::InvalidConfigValue(
                "OAuth authorization-server metadata issuer must use http:// or https://"
                    .to_string(),
            ));
        }
        let expected = infer_issuer_from_metadata_url(&metadata_url);
        let matches = match (&expected, &metadata_origin) {
            (Some(expected), _) => issuers_match(expected, &issuer),
            (None, Some(origin)) => issuers_match(origin, &issuer),
            (None, None) => true,
        };
        if !skip_validation && !matches {
            let expected_text =
                expected.unwrap_or_else(|| metadata_origin.clone().unwrap_or_default());
            return Err(AdapterError::InvalidConfigValue(format!(
                "OAuth authorization-server metadata issuer does not match authServerMetadataUrl: expected {expected_text}"
            )));
        }
        return Ok(metadata);
    }

    if !skip_validation {
        if let Some(origin) = metadata_origin.as_deref() {
            if !issuers_match(&metadata.issuer, origin) {
                return Err(AdapterError::InvalidConfigValue(format!(
                    "auth server issuer mismatch: {origin} vs {}",
                    metadata.issuer
                )));
            }
        }
    }

    Ok(metadata)
}

/// `inferIssuerFromMetadataUrl` (mcp-oauth-provider.ts:169-184 @
/// 10a45367): RFC 8414/OIDC well-known path markers carry the issuer path.
fn infer_issuer_from_metadata_url(metadata_url: &str) -> Option<String> {
    let parsed = url::Url::parse(metadata_url).ok()?;
    let mut pathname = parsed.path().to_string();
    while pathname.len() > 1 && pathname.ends_with('/') {
        pathname.pop();
    }
    if pathname.is_empty() {
        pathname.push('/');
    }
    let origin = parsed.origin().ascii_serialization();
    let oauth_prefix = "/.well-known/oauth-authorization-server";
    if pathname == oauth_prefix || pathname.starts_with(&format!("{oauth_prefix}/")) {
        let issuer_path = pathname.strip_prefix(oauth_prefix).unwrap_or("");
        let issuer_path = if issuer_path.is_empty() {
            "/"
        } else {
            issuer_path
        };
        return join_url(&origin, issuer_path);
    }
    let oidc_path = "/.well-known/openid-configuration";
    if pathname == oidc_path || pathname.starts_with(&format!("{oidc_path}/")) {
        let issuer_path = pathname.strip_prefix(oidc_path).unwrap_or("");
        let issuer_path = if issuer_path.is_empty() {
            "/"
        } else {
            issuer_path
        };
        return join_url(&origin, issuer_path);
    }
    if pathname.ends_with(oidc_path) {
        let issuer_path = &pathname[..pathname.len() - oidc_path.len()];
        let issuer_path = if issuer_path.is_empty() {
            "/"
        } else {
            issuer_path
        };
        return join_url(&origin, issuer_path);
    }
    None
}

/// `new URL(issuerPath, url.origin).toString()` for the absolute/relative
/// path join in [`infer_issuer_from_metadata_url`].
fn join_url(origin: &str, issuer_path: &str) -> Option<String> {
    let base = url::Url::parse(origin).ok()?;
    let joined = base.join(issuer_path).ok()?;
    Some(joined.to_string())
}

/// `issuersMatch` (mcp-oauth-provider.ts:68-72): exact equality modulo a
/// single trailing slash on either side. A plain prefix check would let
/// `https://origin.attacker.tld` pass an `https://origin` expectation.
fn issuers_match(first: &str, second: &str) -> bool {
    first == second
        || (first.ends_with('/') && first[..first.len() - 1] == *second)
        || (second.ends_with('/') && second[..second.len() - 1] == *first)
}

/// `McpOAuthConfig` (types.ts OAuthConfig sub-keys).
struct OAuthConfig {
    client_id: Option<String>,
    client_secret: Option<String>,
    scope: Option<String>,
    grant_type: String,
    redirect_uri: Option<String>,
}

/// The #458 validation ladder, message-for-message
/// (mcp-auth-flow.ts:243-261).
fn validate_auth_server_metadata_url(raw: &Value) -> Result<Option<String>, AdapterError> {
    if raw.is_null() {
        return Ok(None);
    }
    let Some(text) = raw.as_str() else {
        return Err(AdapterError::InvalidConfigValue(
            "OAuth authServerMetadataUrl must be a string".to_string(),
        ));
    };
    let interpolated = crate::utils::interpolate_env_vars(text);
    let trimmed = interpolated.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::InvalidConfigValue(
            "OAuth authServerMetadataUrl must not be empty".to_string(),
        ));
    }
    match url::Url::parse(trimmed) {
        Err(_) => Err(AdapterError::InvalidConfigValue(
            "OAuth authServerMetadataUrl must be an absolute https:// URL".to_string(),
        )),
        Ok(parsed) if parsed.scheme() != "https" => Err(AdapterError::InvalidConfigValue(
            "OAuth authServerMetadataUrl must be an absolute https:// URL".to_string(),
        )),
        Ok(_) => Ok(Some(trimmed.to_string())),
    }
}

fn parse_oauth_config(definition: &ServerEntry) -> OAuthConfig {
    let oauth = definition.get("oauth");
    OAuthConfig {
        client_id: oauth
            .and_then(|o| o.get("clientId"))
            .and_then(Value::as_str)
            .map(str::to_string),
        client_secret: oauth
            .and_then(|o| o.get("clientSecret"))
            .and_then(Value::as_str)
            .map(str::to_string),
        scope: oauth
            .and_then(|o| o.get("scope"))
            .and_then(Value::as_str)
            .map(str::to_string),
        grant_type: oauth
            .and_then(|o| o.get("grantType"))
            .and_then(Value::as_str)
            .unwrap_or("authorization_code")
            .to_string(),
        redirect_uri: oauth
            .and_then(|o| o.get("redirectUri"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// `config.authServerMetadataUrl` (#458): the VALIDATED value consumed by
/// the discovery sites (invalid configs fail the auth path with the
/// upstream messages instead of being silently ignored).
fn configured_auth_server_metadata_url(
    definition: &ServerEntry,
) -> Result<Option<String>, AdapterError> {
    match definition
        .get("oauth")
        .and_then(|o| o.get("authServerMetadataUrl"))
    {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => validate_auth_server_metadata_url(raw),
    }
}

/// The server entry's `oauth.grantType`, defaulting to
/// `authorization_code` (proxy-modes.ts:120 `attemptAutoAuth`).
pub fn configured_grant_type(definition: &ServerEntry) -> String {
    parse_oauth_config(definition).grant_type
}

/// Dynamic Client Registration (RFC 7591): register a new client when
/// `clientId` is not configured. `callback_port` is the bound callback
/// listener's port — the registered `redirect_uris` MUST match it (an
/// OS-assigned port resolves to its real value here).
async fn register_client(
    metadata: &AuthServerMetadata,
    _server_name: &str,
    callback_port: u16,
) -> Result<(String, Option<String>), AdapterError> {
    let endpoint = metadata.registration_endpoint.as_ref().ok_or_else(|| {
        AdapterError::InvalidConfigValue(
            "auth server does not support dynamic client registration".to_string(),
        )
    })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AdapterError::InvalidConfigValue(format!("HTTP client: {e}")))?;

    let redirect_uris = json!([format!(
        "http://localhost:{callback_port}{DEFAULT_OAUTH_CALLBACK_PATH}"
    )]);

    // clientMetadata (mcp-oauth-provider.ts:230-246): field order mirrored
    // for byte-level parity of the recorded request body. `client_uri` is
    // the rpi homepage [VARIANT: upstream ships the adapter repo URL on
    // stock pi; rpi has its own product home].
    let body = json!({
        "redirect_uris": redirect_uris,
        "client_name": "rpi",
        "client_uri": "https://rpi.dev",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });

    let response = client
        .post(endpoint)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("DCR request: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AdapterError::InvalidConfigValue(format!(
            "DCR failed ({status}): {text}"
        )));
    }

    let result: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("DCR response parse: {e}")))?;

    let client_id = result
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AdapterError::InvalidConfigValue("DCR response missing client_id".to_string())
        })?
        .to_string();
    let client_secret = result
        .get("client_secret")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok((client_id, client_secret))
}

/// The `client_credentials` grant flow (no browser, machine-to-machine).
async fn authenticate_client_credentials(
    store: &OAuthCredentialStore,
    server_name: &str,
    server_url: &str,
    _definition: &ServerEntry,
    metadata: &AuthServerMetadata,
    config: &OAuthConfig,
) -> Result<AuthStatus, AdapterError> {
    let client_id = match &config.client_id {
        Some(id) => id.clone(),
        None => {
            // Register dynamically when no client id is configured or
            // stored (client_credentials has no callback listener;
            // redirect_uris are unused for this grant). Errors propagate —
            // an empty client_id must never silently reach the token
            // endpoint (no block_on, no unwrap_or_default).
            let stored_id = store
                .get_entry(server_name)?
                .and_then(|entry| entry.client_info.map(|c| c.client_id));
            match stored_id {
                Some(id) => id,
                None => {
                    let (id, secret) = register_client(metadata, server_name, 0).await?;
                    // Persist the registration so refreshes can use the
                    // DCR-issued client_id (same write as the no-entry
                    // branch — the entry branch previously lost it).
                    store.update_client_info(
                        server_name,
                        store::StoredClientInfo {
                            client_id: id.clone(),
                            client_secret: secret,
                            ..Default::default()
                        },
                        Some(server_url),
                    )?;
                    id
                }
            }
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AdapterError::InvalidConfigValue(format!("HTTP client: {e}")))?;

    let mut body = json!({
        "grant_type": "client_credentials",
        "client_id": client_id,
    });
    if let Some(secret) = &config.client_secret {
        body["client_secret"] = json!(secret);
    }
    if let Some(scope) = &config.scope {
        body["scope"] = json!(scope);
    }

    let response = client
        .post(&metadata.token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .form(&body)
        .send()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("token request: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(AdapterError::InvalidConfigValue(format!(
            "token request failed ({status})"
        )));
    }

    let token_response: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("token response parse: {e}")))?;

    let access_token = token_response
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AdapterError::InvalidConfigValue("token response missing access_token".to_string())
        })?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let expires_at = token_response
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|secs| now + secs as f64);

    let tokens = StoredTokens {
        access_token: access_token.to_string(),
        refresh_token: token_response
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        expires_at,
        scope: token_response
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_string),
        issuer: Some(metadata.issuer.clone()),
    };

    store.update_tokens(server_name, tokens, Some(server_url))?;
    Ok(AuthStatus::Authenticated)
}

/// Build the authorization URL for the authorization-code+PKCE flow.
/// Returns `(url, code_verifier, state)`.
pub fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<(String, String, String), AdapterError> {
    let verifier = generate_code_verifier()?;
    let challenge = code_challenge(&verifier);
    let state = generate_state()?;

    let mut params: Vec<(String, String)> = vec![
        ("response_type".to_string(), "code".to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        ("code_challenge".to_string(), challenge),
        (
            "code_challenge_method".to_string(),
            PKCE_CHALLENGE_METHOD.to_string(),
        ),
        ("state".to_string(), state.clone()),
    ];
    if let Some(scope) = scope {
        params.push(("scope".to_string(), scope.to_string()));
    }

    let separator = if authorization_endpoint.contains('?') {
        "&"
    } else {
        "?"
    };
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
        .collect::<Vec<_>>()
        .join("&");

    let url = format!("{authorization_endpoint}{separator}{query}");
    Ok((url, verifier, state))
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

/// `application/x-www-form-urlencoded` decoding (`+` → space, `%XX` → byte)
/// for the callback query string — the inverse of `urlencoding` and the
/// equivalent of upstream `url.searchParams.get`.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).and_then(|h| {
                    std::str::from_utf8(h)
                        .ok()
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                });
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Cap on the `error_description` text embedded in error messages —
/// hardening: keeps a hostile authorization server from flooding error
/// surfaces (upstream passes it through verbatim).
const MAX_ERROR_DESCRIPTION_CHARS: usize = 200;

/// `authenticate` (mcp-auth-flow.ts): high-level entry point. Dispatches to
/// `client_credentials` or `authorization_code` based on config. Builds the
/// production (OS-keyring) credential store; tests/parity drive
/// [`authenticate_with_store`] with an injected backend.
pub async fn authenticate(
    server_name: &str,
    server_url: &str,
    definition: &ServerEntry,
    options: &AuthenticateOptions,
) -> Result<AuthStatus, AdapterError> {
    let store = OAuthCredentialStore::new(options.auth_storage_options.clone());
    authenticate_with_store(&store, server_name, server_url, definition, options).await
}

/// Store-injecting `authenticate` body (test/parity hook; same semantics).
pub async fn authenticate_with_store(
    store: &OAuthCredentialStore,
    server_name: &str,
    server_url: &str,
    definition: &ServerEntry,
    options: &AuthenticateOptions,
) -> Result<AuthStatus, AdapterError> {
    let config = parse_oauth_config(definition);

    // Check existing credentials first.
    if let Some(entry) = store.get_for_url(server_name, server_url)? {
        if let Some(tokens) = &entry.tokens {
            // Check expiry
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let expired = tokens.expires_at.is_some_and(|exp| exp < now);
            if !expired {
                return Ok(AuthStatus::Authenticated);
            }
            // Try refresh token
            if let Some(refresh) = &tokens.refresh_token {
                if let Some(issuer) = &tokens.issuer {
                    let metadata_override = configured_auth_server_metadata_url(definition)
                        .ok()
                        .flatten();
                    if let Ok(metadata) = discover_auth_server_metadata_with_override(
                        server_url,
                        options.skip_issuer_metadata_validation,
                        metadata_override.as_deref(),
                    )
                    .await
                    {
                        // DCR-registered clients: prefer the configured
                        // client id, fall back to the stored registration —
                        // config.client_id alone would silently fail the
                        // refresh for dynamically registered clients.
                        let refresh_client_id = config
                            .client_id
                            .clone()
                            .or_else(|| entry.client_info.as_ref().map(|c| c.client_id.clone()));
                        let refresh_client_secret = config.client_secret.clone().or_else(|| {
                            entry
                                .client_info
                                .as_ref()
                                .and_then(|c| c.client_secret.clone())
                        });
                        match refresh_token(
                            &metadata.token_endpoint,
                            &refresh_client_id,
                            refresh,
                            &refresh_client_secret,
                        )
                        .await
                        {
                            Ok(new_tokens) => {
                                store.update_tokens(server_name, new_tokens, Some(server_url))?;
                                return Ok(AuthStatus::Authenticated);
                            }
                            // #503: only an `invalid_grant` rejection means the
                            // stored dynamic client is stale — drop the
                            // registration so the interactive leg below
                            // re-registers with the current callback URI.
                            // Transient failures (network/parse/other error
                            // codes) keep the registration for a later retry.
                            Err(AdapterError::OAuthInvalidGrant) => {
                                let _ = store.clear_client_info(server_name);
                            }
                            Err(_) => {}
                        }
                    }
                    let _ = issuer;
                }
            }
        }
    }

    let metadata_override = configured_auth_server_metadata_url(definition)?;
    let metadata = discover_auth_server_metadata_with_override(
        server_url,
        options.skip_issuer_metadata_validation,
        metadata_override.as_deref(),
    )
    .await?;

    if config.grant_type == "client_credentials" {
        return authenticate_client_credentials(
            store,
            server_name,
            server_url,
            definition,
            &metadata,
            &config,
        )
        .await;
    }

    // Authorization code + PKCE flow
    // Bind the callback listener FIRST (mcp-callback-server.ts
    // ensureCallbackServer): an OS-assigned port (0) must be resolved to the
    // real one before DCR and the authorization URL are built — the
    // registered redirect_uris and the redirect_uri in the authorization
    // URL have to match the bound listener exactly.
    let port = DEFAULT_OAUTH_CALLBACK_PORT;
    let callback_listener = bind_callback_listener(port).await?;
    let actual_port = callback_listener
        .local_addr()
        .map_err(|e| AdapterError::InvalidConfigValue(format!("callback server addr: {e}")))?
        .port();

    let client_id;
    let mut client_secret = config.client_secret.clone();
    match &config.client_id {
        Some(id) => client_id = id.clone(),
        None => {
            let (id, secret) = register_client(&metadata, server_name, actual_port).await?;
            store.update_client_info(
                server_name,
                store::StoredClientInfo {
                    client_id: id.clone(),
                    client_secret: secret.clone(),
                    ..Default::default()
                },
                Some(server_url),
            )?;
            client_id = id;
            // DCR-issued secret authenticates the token exchange even
            // though it is not in the config (upstream stores it on the
            // provider and the SDK applies it automatically).
            if client_secret.is_none() {
                client_secret = secret;
            }
        }
    };

    let redirect_uri = config
        .redirect_uri
        .clone()
        .unwrap_or_else(|| format!("http://localhost:{actual_port}{DEFAULT_OAUTH_CALLBACK_PATH}"));

    let (auth_url, code_verifier, state) = build_authorization_url(
        &metadata.authorization_endpoint,
        &client_id,
        &redirect_uri,
        config.scope.as_deref(),
    )?;

    // Save PKCE state.
    store.update_code_verifier(server_name, code_verifier.clone(), Some(server_url))?;
    store.update_oauth_state(server_name, state.clone(), Some(server_url))?;

    // Notify the caller with the authorization URL.
    if let Some(cb) = &options.on_authorization_url {
        cb(&auth_url);
    }

    // Start the callback server and wait for the redirect.
    let callback_result =
        wait_for_callback(callback_listener, &state, options.signal.clone()).await?;

    // RFC 9207 issuer validation (mcp-auth-flow.ts:674-685): when `iss` is
    // present it must equal the discovered issuer; when the metadata
    // advertises `authorization_response_iss_parameter_supported`, the
    // callback MUST carry it. Upstream compares for exact equality.
    if let Some(iss) = &callback_result.iss {
        if *iss != metadata.issuer {
            return Err(AdapterError::InvalidConfigValue(format!(
                "The OAuth authorization response issuer does not match the discovered issuer for {server_name}"
            )));
        }
    } else if metadata.authorization_response_iss_parameter_supported == Some(true) {
        return Err(AdapterError::InvalidConfigValue(format!(
            "The authorization server for {server_name} requires the RFC 9207 \"iss\" parameter"
        )));
    }

    // Exchange the authorization code for tokens. The verifier prefers the
    // in-memory value (the store round-trip is for cross-process resume).
    let code_verifier = store
        .get_entry(server_name)?
        .and_then(|e| e.code_verifier)
        .unwrap_or(code_verifier);

    let tokens = exchange_code(
        &metadata.token_endpoint,
        &client_id,
        &callback_result.code,
        &redirect_uri,
        &code_verifier,
        client_secret.as_ref(),
    )
    .await?;

    store.update_tokens(server_name, tokens, Some(server_url))?;

    Ok(AuthStatus::Authenticated)
}

/// Exchange an authorization code for tokens (RFC 6749 §4.1.3 + PKCE).
async fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    client_secret: Option<&String>,
) -> Result<StoredTokens, AdapterError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AdapterError::InvalidConfigValue(format!("HTTP client: {e}")))?;

    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.as_str()));
    }

    let response = client
        .post(token_endpoint)
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("token exchange: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(AdapterError::InvalidConfigValue(format!(
            "token exchange failed ({status})"
        )));
    }

    let token_response: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("token response parse: {e}")))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    Ok(StoredTokens {
        access_token: token_response
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AdapterError::InvalidConfigValue("token response missing access_token".to_string())
            })?
            .to_string(),
        refresh_token: token_response
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        expires_at: token_response
            .get("expires_in")
            .and_then(Value::as_u64)
            .map(|secs| now + secs as f64),
        scope: token_response
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_string),
        issuer: None,
    })
}

/// Refresh an access token using a refresh token (RFC 6749 §6).
async fn refresh_token(
    token_endpoint: &str,
    client_id: &Option<String>,
    refresh_token: &str,
    client_secret: &Option<String>,
) -> Result<StoredTokens, AdapterError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AdapterError::InvalidConfigValue(format!("HTTP client: {e}")))?;

    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    if let Some(id) = client_id {
        form.push(("client_id", id.clone()));
    }
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.clone()));
    }

    let response = client
        .post(token_endpoint)
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("token refresh: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        // RFC 6749 §5.2 error object: `invalid_grant` means the refresh token
        // (or its bound dynamic client registration) is stale (#503).
        let error_code = serde_json::from_str::<Value>(&body).ok().and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if error_code.as_deref() == Some("invalid_grant") {
            return Err(AdapterError::OAuthInvalidGrant);
        }
        return Err(AdapterError::InvalidConfigValue(match error_code {
            Some(code) => format!("token refresh failed ({status}): {code}"),
            None => format!("token refresh failed ({status})"),
        }));
    }

    let token_response: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("token refresh parse: {e}")))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    Ok(StoredTokens {
        access_token: token_response
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        refresh_token: token_response
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(refresh_token.to_string())),
        expires_at: token_response
            .get("expires_in")
            .and_then(Value::as_u64)
            .map(|secs| now + secs as f64),
        scope: token_response
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_string),
        issuer: None,
    })
}

/// Localhost callback server: binds `127.0.0.1:{port}`, waits for a single
/// redirect with `?code=...&state=...`, then closes. Uses
/// `tokio::net::TcpListener` (design §3.7: no axum).
///
/// Upstream `mcp-callback-server.ts`: the server is a singleton with
/// pending-auth state tracking. Here we simplify to a one-shot server per
/// call (sufficient for the P1-wave automated and manual flows).
async fn bind_callback_listener(port: u16) -> Result<tokio::net::TcpListener, AdapterError> {
    let bind_addr = if port == 0 {
        "127.0.0.1:0".to_string()
    } else {
        format!("127.0.0.1:{port}")
    };
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("callback server bind: {e}")))?;
    tracing::debug!(
        port = listener.local_addr().map(|a| a.port()).unwrap_or(0),
        "MCP OAuth callback server listening"
    );
    Ok(listener)
}

async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<CallbackResult, AdapterError> {
    let deadline = tokio::time::sleep(CALLBACK_TIMEOUT);
    let cancel = cancel.unwrap_or_default();

    tokio::select! {
        _ = cancel.cancelled() => {
            Err(AdapterError::InvalidConfigValue("OAuth callback cancelled".to_string()))
        }
        _ = deadline => {
            Err(AdapterError::InvalidConfigValue("OAuth callback timeout - authorization took too long".to_string()))
        }
        result = accept_callback(&listener, expected_state) => {
            result
        }
    }
}

/// Accept a single HTTP connection, parse the callback query string, validate
/// state, and return the authorization code.
async fn accept_callback(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
) -> Result<CallbackResult, AdapterError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut socket, _) = listener
        .accept()
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("callback accept: {e}")))?;

    let mut buf = vec![0u8; 4096];
    let n = socket
        .read(&mut buf)
        .await
        .map_err(|e| AdapterError::InvalidConfigValue(format!("callback read: {e}")))?;

    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the request line: GET /oauth/callback?code=xxx&state=yyy HTTP/1.1
    let request_line = request.lines().next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("");

    // Parse query string. Percent-decode keys/values (`+` → space) to match
    // upstream `url.searchParams.get` (mcp-callback-server.ts:215-219) —
    // the raw slice would corrupt codes containing `%2B` etc.
    let query = path.split('?').nth(1).unwrap_or("");
    let params: HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect();

    // Validate state (CSRF protection).
    let state = params.get("state").map(String::as_str).unwrap_or("");
    if state != expected_state {
        let _ = socket
            .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: 0\r\n\r\n")
            .await;
        return Err(AdapterError::InvalidConfigValue(
            "Missing or invalid state parameter - potential CSRF attack".to_string(),
        ));
    }

    // Check for error response.
    if let Some(error) = params.get("error") {
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 0\r\n\r\n")
            .await;
        // Upstream surfaces `${error}: ${description}` verbatim
        // (mcp-auth-flow.ts:536-538); the description is capped here so a
        // hostile AS cannot flood error surfaces (hardening, not upstream
        // parity).
        let message = match params.get("error_description") {
            Some(description) => {
                let truncated: String = description
                    .chars()
                    .take(MAX_ERROR_DESCRIPTION_CHARS)
                    .collect();
                format!("{error}: {truncated}")
            }
            None => error.clone(),
        };
        return Err(AdapterError::InvalidConfigValue(message));
    }

    let code = params.get("code").cloned().unwrap_or_default();
    if code.is_empty() {
        let _ = socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/html\r\ncontent-length: 0\r\n\r\n",
            )
            .await;
        return Err(AdapterError::InvalidConfigValue(
            "No authorization code provided".to_string(),
        ));
    }

    let iss = params.get("iss").cloned();

    // Send success response.
    let body = "<!DOCTYPE html><html><body><h1>Authorization Successful</h1><p>You can close this window.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;

    Ok(CallbackResult {
        code: code.to_string(),
        iss,
    })
}

/// `removeAuth` (mcp-auth-flow.ts): clear all stored credentials.
pub fn remove_auth(server_name: &str, options: &AuthStorageOptions) -> Result<(), AdapterError> {
    let store = OAuthCredentialStore::new(options.clone());
    store.remove_entry(server_name)
}

/// #422/#423 compare-and-delete: remove the stored credential only when its
/// access token equals the one that just failed. A token another process
/// wrote after our request is preserved. Returns whether the entry was
/// removed. Token values never reach logs (G4 red line).
pub fn remove_auth_if_token_matches(
    store: &OAuthCredentialStore,
    server_name: &str,
    server_url: &str,
    invalidated_token: &str,
) -> Result<bool, AdapterError> {
    let Some(entry) = store.get_for_url(server_name, server_url)? else {
        return Ok(false);
    };
    let Some(tokens) = entry.tokens else {
        return Ok(false);
    };
    if tokens.access_token != invalidated_token {
        return Ok(false);
    }
    store.remove_entry(server_name)?;
    Ok(true)
}

/// Resolve a usable access token for a server (FR-P1-04, connect-path
/// injection): the stored token when still valid, or a refresh-token
/// exchange when expired. Mirrors the SDK `auth()` semantics on the
/// upstream connect path — a stored token rides the request as
/// `Authorization: Bearer` without waiting for a 401 round-trip.
///
/// The returned value is credential material and MUST NOT be logged (G4).
/// `None` means "no usable token" — the caller connects unauthenticated and
/// the 401 → needs-auth flow takes over.
pub async fn resolve_access_token(
    store: &OAuthCredentialStore,
    server_name: &str,
    server_url: &str,
    definition: &ServerEntry,
) -> Result<Option<String>, AdapterError> {
    let Some(entry) = store.get_for_url(server_name, server_url)? else {
        return Ok(None);
    };
    let Some(tokens) = entry.tokens else {
        return Ok(None);
    };
    if tokens.access_token.is_empty() {
        return Ok(None);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let expired = tokens.expires_at.is_some_and(|exp| exp < now);
    if !expired {
        return Ok(Some(tokens.access_token));
    }

    // Expired with a refresh token: exchange it (RFC 6749 §6) and persist
    // the new tokens. A failed refresh degrades to None (the needs-auth
    // flow re-runs the full grant).
    let Some(refresh) = tokens.refresh_token else {
        return Ok(None);
    };
    let metadata_override = configured_auth_server_metadata_url(definition)
        .ok()
        .flatten();
    let metadata = match discover_auth_server_metadata_with_override(
        server_url,
        false,
        metadata_override.as_deref(),
    )
    .await
    {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::debug!(server = server_name, %error, "OAuth token refresh metadata discovery failed");
            return Ok(None);
        }
    };
    // DCR-registered clients: prefer the configured client id, fall back to
    // the stored registration (same precedence as the refresh path in
    // `authenticate`).
    let config = parse_oauth_config(definition);
    let client_id = config
        .client_id
        .or_else(|| entry.client_info.as_ref().map(|c| c.client_id.clone()));
    let client_secret = config.client_secret.or_else(|| {
        entry
            .client_info
            .as_ref()
            .and_then(|c| c.client_secret.clone())
    });
    match refresh_token(
        &metadata.token_endpoint,
        &client_id,
        &refresh,
        &client_secret,
    )
    .await
    {
        Ok(new_tokens) => {
            let access_token = new_tokens.access_token.clone();
            store.update_tokens(server_name, new_tokens, Some(server_url))?;
            Ok((!access_token.is_empty()).then_some(access_token))
        }
        Err(error @ AdapterError::OAuthInvalidGrant) => {
            // #503: the refresh token is dead and the dynamic client bound to
            // it may be stale — drop the registration so the next
            // interactive flow re-registers against the current callback.
            tracing::debug!(server = server_name, %error, "OAuth token refresh rejected; dropping stale client registration");
            let _ = store.clear_client_info(server_name);
            Ok(None)
        }
        Err(error) => {
            tracing::debug!(server = server_name, %error, "OAuth token refresh failed");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_verifier_is_valid_length() {
        let verifier = generate_code_verifier().expect("CSPRNG available on Linux");
        // RFC 7636: 43-128 chars
        assert!(verifier.len() >= 43 && verifier.len() <= 128);
        // Only unreserved chars
        assert!(verifier.chars().all(|c| c.is_ascii_alphanumeric()
            || c == '-'
            || c == '.'
            || c == '_'
            || c == '~'));
    }

    #[test]
    fn code_challenge_is_base64url_of_sha256() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = code_challenge(verifier);
        // Known PKCE test vector from RFC 7636 Appendix B.
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn state_generation_is_unique() {
        let s1 = generate_state().expect("CSPRNG");
        let s2 = generate_state().expect("CSPRNG");
        assert_ne!(s1, s2);
        assert!(s1.len() >= 32);
    }

    #[test]
    fn authorization_url_contains_required_params() {
        let (url, verifier, state) = build_authorization_url(
            "https://auth.test/authorize",
            "client-123",
            "http://localhost:0/callback",
            Some("read write"),
        )
        .expect("CSPRNG");
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains(&format!("state={state}")));
        assert!(url.contains("scope=read%20write"));
        assert!(!verifier.is_empty());
    }

    #[test]
    fn urlencoding_handles_special_chars() {
        assert_eq!(urlencoding("hello world"), "hello%20world");
        assert_eq!(urlencoding("a+b=c"), "a%2Bb%3Dc");
        assert_eq!(urlencoding("safe-_.~"), "safe-_.~");
    }

    #[test]
    fn parse_oauth_config_defaults() {
        let entry = ServerEntry(
            json!({ "url": "https://test/mcp" })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let config = parse_oauth_config(&entry);
        assert_eq!(config.grant_type, "authorization_code");
        assert!(config.client_id.is_none());
    }

    #[test]
    fn parse_oauth_config_with_custom_values() {
        let entry = ServerEntry(
            json!({
                "url": "https://test/mcp",
                "oauth": {
                    "clientId": "my-client",
                    "clientSecret": "secret",
                    "scope": "read",
                    "grantType": "client_credentials"
                }
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        let config = parse_oauth_config(&entry);
        assert_eq!(config.client_id.as_deref(), Some("my-client"));
        assert_eq!(config.client_secret.as_deref(), Some("secret"));
        assert_eq!(config.scope.as_deref(), Some("read"));
        assert_eq!(config.grant_type, "client_credentials");
    }

    #[test]
    fn configured_grant_type_matches_attempt_auto_auth_guard() {
        // attemptAutoAuth (proxy-modes.ts:120): only client_credentials may
        // proceed headless; the default must read authorization_code.
        let plain = ServerEntry(
            json!({ "url": "https://test/mcp" })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        assert_eq!(configured_grant_type(&plain), "authorization_code");
        let cc = ServerEntry(
            json!({ "url": "https://test/mcp", "oauth": { "grantType": "client_credentials" } })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        assert_eq!(configured_grant_type(&cc), "client_credentials");
    }

    #[test]
    fn percent_decode_round_trips_and_handles_plus() {
        assert_eq!(percent_decode("read%20write"), "read write");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("abc%2Bdef"), "abc+def");
        // Malformed escapes pass through unchanged.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn issuers_match_is_exact_modulo_trailing_slash() {
        assert!(issuers_match("https://a.test", "https://a.test"));
        assert!(issuers_match("https://a.test/", "https://a.test"));
        assert!(issuers_match("https://a.test", "https://a.test/"));
        // A prefix must NOT pass: https://a.test.attacker.tld.
        assert!(!issuers_match(
            "https://a.test.attacker.tld",
            "https://a.test"
        ));
    }
    #[test]
    fn auth_server_metadata_url_validation_ladder() {
        // #458 (mcp-auth-flow.ts:243-261 @ 10a45367).
        let entry = |value: serde_json::Value| {
            ServerEntry(
                serde_json::json!({ "url": "https://a.test/mcp", "oauth": { "authServerMetadataUrl": value } })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            )
        };
        let invalid = |value: serde_json::Value, message: &str| {
            let error = configured_auth_server_metadata_url(&entry(value)).unwrap_err();
            assert!(
                error.to_string().ends_with(message),
                "expected ...{message}, got {}",
                error
            );
        };
        invalid(
            serde_json::json!(42),
            "OAuth authServerMetadataUrl must be a string",
        );
        invalid(
            serde_json::json!("  "),
            "OAuth authServerMetadataUrl must not be empty",
        );
        invalid(
            serde_json::json!("not a url"),
            "OAuth authServerMetadataUrl must be an absolute https:// URL",
        );
        invalid(
            serde_json::json!("http://a.test/.well-known/oauth-authorization-server"),
            "OAuth authServerMetadataUrl must be an absolute https:// URL",
        );
        let ok = configured_auth_server_metadata_url(&entry(serde_json::json!(
            " https://idp.test/.well-known/oauth-authorization-server "
        )))
        .expect("valid")
        .expect("some");
        assert_eq!(
            ok,
            "https://idp.test/.well-known/oauth-authorization-server"
        );
        let absent = configured_auth_server_metadata_url(&ServerEntry(
            serde_json::json!({ "url": "https://a.test/mcp" })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        ))
        .expect("no oauth key");
        assert_eq!(absent, None);
    }

    #[test]
    fn infer_issuer_from_metadata_url_markers() {
        // mcp-oauth-provider.ts:169-184 @ 10a45367.
        assert_eq!(
            infer_issuer_from_metadata_url(
                "https://idp.test/.well-known/oauth-authorization-server"
            )
            .as_deref(),
            Some("https://idp.test/")
        );
        assert_eq!(
            infer_issuer_from_metadata_url(
                "https://idp.test/.well-known/oauth-authorization-server/tenant1"
            )
            .as_deref(),
            Some("https://idp.test/tenant1")
        );
        let oidc_nested = infer_issuer_from_metadata_url(
            "https://idp.test/tenants/a/.well-known/openid-configuration",
        )
        .expect("some");
        assert!(
            oidc_nested == "https://idp.test/tenants/a/"
                || oidc_nested == "https://idp.test/tenants/a",
            "nested oidc issuer: {oidc_nested}"
        );
        assert_eq!(
            infer_issuer_from_metadata_url("https://idp.test/custom/metadata").as_deref(),
            None
        );
    }
}
