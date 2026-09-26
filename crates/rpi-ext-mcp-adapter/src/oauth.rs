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

pub mod encrypted_store;
pub mod store;

pub use encrypted_store::format_oauth_credential_store_unavailable;
pub use store::{CredentialStoreSelection, SecretStoreKind};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::AdapterError;
use crate::metadata::ServerEntry;
use store::{AuthStorageOptions, OAuthCredentialStore, StoredTokens};

/// `resolveOAuthRequestTimeoutMs` (#486, mcp-auth-fetch.ts:38-43):
/// every OAuth discovery/registration/exchange/refresh request is bounded.
/// [VARIANT] rpi default is 10s (stricter than the upstream 30s default —
/// the v0.1.4 port chose 10s and the bound, not the number, is the
/// #486 contract); `RPI_MCP_OAUTH_REQUEST_TIMEOUT_MS` overrides (brand
/// rename per coding-standards §env).
fn oauth_request_timeout() -> Duration {
    std::env::var("RPI_MCP_OAUTH_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(10))
}

/// #535 (0829964, mcp-auth-fetch.ts createOAuthFetch @ 97435aab): the
/// configured `headers` are SERVICE headers for the configured MCP origin
/// only — they never ride along to a discovered issuer, SDK-constructed
/// request headers win, and a credential-bearing request refuses redirects
/// (fail closed, generic error text so the cause cannot leak secrets).
/// Only an explicit `auth: "oauth"` coexists with configured headers
/// (implicit auto-detection is disabled by their presence).
#[derive(Clone)]
struct OAuthFetch {
    server_origin: String,
    headers: Vec<(String, String)>,
    /// #539: the per-server CA client (`caFile`); used for same-origin
    /// requests (redirects already refused on it).
    ca_client: Option<reqwest::Client>,
}

impl OAuthFetch {
    fn new(definition: &ServerEntry, server_url: &str, server_name: &str) -> Self {
        let explicit_oauth = definition.get_str("auth") == Some("oauth");
        let headers = if explicit_oauth {
            crate::utils::resolve_command_secrets_record(definition.get("headers"), &|key| {
                format!("MCP server \"{server_name}\" OAuth HTTP header \"{key}\"")
            })
            .unwrap_or_default()
            .into_iter()
            .flat_map(|map| {
                map.into_iter()
                    .filter_map(|(key, value)| value.as_str().map(|text| (key, text.to_string())))
            })
            .collect()
        } else {
            Vec::new()
        };
        let origin = url::Url::parse(server_url)
            .map(|parsed| {
                let port_suffix = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
                format!(
                    "{}://{}{}",
                    parsed.scheme(),
                    parsed.host_str().unwrap_or("localhost"),
                    port_suffix
                )
            })
            .unwrap_or_else(|_| server_url.to_string());
        // #539: connection CA trust applies to the OAuth provider requests
        // (same origin). An unloadable bundle fails the connect boundary
        // first; here a failure degrades to the system trust store.
        // #486 (P1 review fix): this OAuth-side CA client carries the OAuth
        // request timeout — the bounded-request invariant covers EVERY
        // OAuth fetch; the transport's own CA client stays timeout-free
        // (streaming legs must not inherit a request deadline).
        let ca_client = crate::protocol::http::load_ca_bundle(definition, server_name)
            .ok()
            .and_then(|certificates| {
                let mut builder = reqwest::Client::builder()
                    .timeout(oauth_request_timeout())
                    .redirect(reqwest::redirect::Policy::none());
                for certificate in certificates {
                    builder = builder.add_root_certificate(certificate);
                }
                builder.build().ok()
            });
        Self {
            server_origin: origin,
            headers,
            ca_client,
        }
    }

    fn protected(&self, target: &str) -> bool {
        !self.headers.is_empty() && Self::origin_of(target) == Some(self.server_origin.clone())
    }

    fn origin_of(target: &str) -> Option<String> {
        let parsed = url::Url::parse(target).ok()?;
        let port_suffix = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
        Some(format!(
            "{}://{}{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or("localhost"),
            port_suffix
        ))
    }

    /// The service headers for this target (same-origin only).
    fn service_headers(&self, target: &str) -> Vec<(String, String)> {
        if self.protected(target) {
            self.headers.clone()
        } else {
            Vec::new()
        }
    }

    /// A client for this target: same-origin requests prefer the #539 CA
    /// client; redirects are refused while service headers are attached
    /// (no trust-bearing redirect hops).
    fn client(&self, target: &str) -> Result<reqwest::Client, AdapterError> {
        let same_origin = Self::origin_of(target).as_deref() == Some(self.server_origin.as_str());
        if same_origin && self.ca_client.is_some() {
            return Ok(self.ca_client.clone().unwrap_or_default());
        }
        let mut builder = reqwest::Client::builder().timeout(oauth_request_timeout());
        if self.protected(target) {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        builder
            .build()
            .map_err(|e| AdapterError::InvalidConfigValue(format!("HTTP client: {e}")))
    }

    /// `#535` / `createOAuthFetch` (mcp-auth-fetch.ts:69-80): every request
    /// through this fetch carries the same-origin service headers, and
    /// request-specific headers WIN over them (upstream builds
    /// `new Headers(serviceHeaders)` then `set`s the request headers —
    /// reqwest's `header()` would append instead).
    fn request_headers(
        &self,
        target: &str,
        specific: &[(&str, &str)],
    ) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut put = |key: &str, value: &str| {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        };
        for (key, value) in self.service_headers(target) {
            put(&key, &value);
        }
        for (key, value) in specific {
            put(key, value);
        }
        headers
    }

    /// `#535`: a failed protected request reports a generic cause.
    fn map_send_error(&self, target: &str, context: &str, error: reqwest::Error) -> AdapterError {
        if self.protected(target) {
            AdapterError::InvalidConfigValue("OAuth HTTP request failed".to_string())
        } else {
            AdapterError::InvalidConfigValue(format!("{context}: {error}"))
        }
    }
}

/// `MODERN_PROTOCOL_VERSION` (mcp-probe.ts:2).
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// `LEGACY_PROTOCOL_VERSION` (mcp-probe.ts:3).
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-06-18";

/// `DEFAULT_OAUTH_CALLBACK_PORT` (mcp-oauth-provider.ts:92): 19876, the
/// fixed loopback port for pre-registered clients relying on the default
/// `/callback` path; overridable via `MCP_OAUTH_CALLBACK_PORT`. (v0.1.4
// ported this as `0` = always OS-assigned; #483 completes the semantics —
// strict binding for pre-registered clients, OS-assigned for dynamic
// clients — closing the drift registered with the #483 anchor.)
pub const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 19876;

/// The configured callback port (`MCP_OAUTH_CALLBACK_PORT`, 1-65535,
/// default 19876).
fn configured_oauth_callback_port() -> u16 {
    std::env::var("MCP_OAUTH_CALLBACK_PORT")
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
        .unwrap_or(DEFAULT_OAUTH_CALLBACK_PORT)
}

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
    // #571 (b8fbc9c): when advertised, an operator-supplied
    // `oauth.clientMetadataUrl` is used as the client_id (CIMD) and Dynamic
    // Client Registration is skipped.
    #[serde(default)]
    client_id_metadata_document_supported: Option<bool>,
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
    fetch: &OAuthFetch,
) -> Result<AuthServerMetadata, AdapterError> {
    // The metadata URL is resolved first so the #535 service-header scope
    // can key off the actual target (same-origin only).
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
    let client = fetch.client(&metadata_url)?;
    let request = client
        .get(&metadata_url)
        .headers(fetch.request_headers(&metadata_url, &[("accept", "application/json")]));
    let response = request
        .send()
        .await
        .map_err(|e| fetch.map_send_error(&metadata_url, "auth server metadata fetch", e))?;

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
    /// `oauth.clientMetadataUrl` (#571): an advanced, operator-supplied
    /// public HTTPS Client ID Metadata Document URL.
    client_metadata_url: Option<String>,
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
        client_metadata_url: oauth
            .and_then(|o| o.get("clientMetadataUrl"))
            .and_then(Value::as_str)
            // #571 (mcp-auth-flow.ts:215): the URL is env-interpolated and
            // trimmed at parse, exactly like `authServerMetadataUrl`.
            .map(|text| crate::utils::interpolate_env_vars(text).trim().to_string()),
    }
}

/// The #571 validation ladder (mcp-oauth-provider.ts constructor @
/// 97435aab): a CIMD URL must be HTTPS with a non-root path; a client
/// secret without an explicit client id cannot be combined with CIMD
/// (ambiguous which credential authorizes the request). An explicit
/// `clientId` always wins (CIMD is then ignored, matching the provider's
/// `clientMetadataUrl` getter).
/// The #503 registration gate (mcp-auth-flow.ts:556-573 @ 97435aab,
/// re-review Finding 2), shared by the reuse filter and the dead-
/// registration cleanup: a registration with NO stored tokens is
/// orphaned — dead; with tokens, it is reusable only when the stored
/// redirect_uris CONTAIN the current callback (an absent list does not
/// match) or the pair is refresh-capable.
fn registration_reusable(
    info: &store::StoredClientInfo,
    has_tokens: bool,
    refresh_capable: bool,
    redirect_uri: &str,
) -> bool {
    if !has_tokens {
        return false;
    }
    let redirect_matches = info
        .redirect_uris
        .as_ref()
        .is_some_and(|uris| uris.contains(&redirect_uri.to_string()));
    redirect_matches || refresh_capable
}

fn validate_oauth_config(config: &OAuthConfig) -> Result<(), AdapterError> {
    if let Some(metadata_url) = &config.client_metadata_url {
        // #571 (mcp-auth-flow.ts:216-218): an env-interpolated URL that
        // trims to empty is rejected (an unset env var must not silently
        // disable/enable CIMD).
        if metadata_url.is_empty() {
            return Err(AdapterError::InvalidConfigValue(
                "OAuth clientMetadataUrl must not be empty".to_string(),
            ));
        }
        // Upstream validates the URL itself first (mcp-auth-flow.ts:219
        // validateClientMetadataUrl), before the clientSecret combination
        // rule from the provider constructor.
        let valid = url::Url::parse(metadata_url)
            .ok()
            .filter(|parsed| {
                parsed.scheme() == "https" && parsed.path() != "/" && !parsed.path().is_empty()
            })
            .is_some();
        if !valid {
            return Err(AdapterError::InvalidConfigValue(
                "clientMetadataUrl must be a valid HTTPS URL with a non-root pathname".to_string(),
            ));
        }
        if config.client_id.is_none() && config.client_secret.is_some() {
            return Err(AdapterError::InvalidConfigValue(
                "OAuth clientSecret requires an explicit clientId when clientMetadataUrl is configured"
                    .to_string(),
            ));
        }
    }
    Ok(())
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
    redirect_uri: &str,
    fetch: &OAuthFetch,
) -> Result<(String, Option<String>), AdapterError> {
    let endpoint = metadata.registration_endpoint.as_ref().ok_or_else(|| {
        AdapterError::InvalidConfigValue(
            "auth server does not support dynamic client registration".to_string(),
        )
    })?;

    let client = fetch.client(endpoint)?;

    let redirect_uris = json!([redirect_uri]);

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

    let request = client.post(endpoint).headers(fetch.request_headers(
        endpoint,
        &[
            ("content-type", "application/json"),
            ("accept", "application/json"),
        ],
    ));
    let response = request
        .json(&body)
        .send()
        .await
        .map_err(|e| fetch.map_send_error(endpoint, "DCR request", e))?;

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
    fetch: &OAuthFetch,
) -> Result<AuthStatus, AdapterError> {
    // mcp-auth-flow.ts:469-474: a token-less stored registration is dead for
    // client_credentials (no interactive grant can have produced it); clear
    // it before resolving identity unless a clientId is configured.
    if config.client_id.is_none() {
        if let Some(entry) = store.get_for_url(server_name, server_url)? {
            if entry.client_info.is_some() && entry.tokens.is_none() {
                let _ = store.clear_client_info(server_name);
            }
        }
    }
    let mut stored_secret: Option<String> = None;
    let client_id = match &config.client_id {
        Some(id) => id.clone(),
        None => {
            // Register dynamically when no client id is configured or
            // stored (client_credentials has no callback listener;
            // redirect_uris are unused for this grant). Errors propagate —
            // an empty client_id must never silently reach the token
            // endpoint (no block_on, no unwrap_or_default). The read is
            // URL-scoped like `getAuthForUrl` (mcp-oauth-provider.ts:399).
            let stored_info = store
                .get_for_url(server_name, server_url)?
                .and_then(|entry| entry.client_info);
            match stored_info {
                Some(info) => {
                    // Upstream forwards the stored registration's secret to
                    // the token endpoint (mcp-oauth-provider.ts:774-784).
                    stored_secret = info.client_secret.clone();
                    info.client_id.clone()
                }
                None => {
                    let default_redirect =
                        format!("http://localhost:0{DEFAULT_OAUTH_CALLBACK_PATH}");
                    let (id, secret) =
                        register_client(metadata, server_name, &default_redirect, fetch).await?;
                    // Persist the registration so refreshes can use the
                    // DCR-issued client_id (same write as the no-entry
                    // branch — the entry branch previously lost it).
                    store.update_client_info(
                        server_name,
                        store::StoredClientInfo {
                            client_id: id.clone(),
                            client_secret: secret.clone(),
                            ..Default::default()
                        },
                        Some(server_url),
                    )?;
                    stored_secret = secret;
                    id
                }
            }
        }
    };

    let client = fetch.client(&metadata.token_endpoint)?;

    let mut body = json!({
        "grant_type": "client_credentials",
        "client_id": client_id,
    });
    if let Some(secret) = config.client_secret.as_ref().or(stored_secret.as_ref()) {
        body["client_secret"] = json!(secret);
    }
    if let Some(scope) = &config.scope {
        body["scope"] = json!(scope);
    }

    let request = client
        .post(&metadata.token_endpoint)
        .headers(fetch.request_headers(
            &metadata.token_endpoint,
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("accept", "application/json"),
            ],
        ));
    let response = request
        .form(&body)
        .send()
        .await
        .map_err(|e| fetch.map_send_error(&metadata.token_endpoint, "token request", e))?;

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
    // #571: the CIMD validation ladder runs at the auth boundary (upstream
    // throws from the McpOAuthProvider constructor).
    validate_oauth_config(&config)?;
    // #535: origin-scoped service headers for this auth leg (explicit
    // `auth: "oauth"` only).
    let fetch = OAuthFetch::new(definition, server_url, server_name);

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
                        &fetch,
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
                            &fetch,
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
        &fetch,
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
            &fetch,
        )
        .await;
    }

    // Authorization code + PKCE flow
    // #483 (cb8e316): the callback endpoint comes from `oauth.redirectUri`
    // when it is a loopback `http://` URI — an explicit port is bound
    // exactly, `{port}` (or no port) takes an OS-assigned port, and the
    // URI's host (localhost / 127.0.0.1 / [::1]) is the bound host. Bind
    // FIRST so the registered redirect_uris and the authorization URL
    // carry the resolved port. A hard-invalid redirectUri fails the flow
    // BEFORE anything binds (#483 upstream error ladder).
    let loopback = match &config.redirect_uri {
        Some(uri) => parse_loopback_redirect_uri(uri)?,
        // Manual mode (https + non-loopback): no loopback target —
        // the default listener + callback window is the existing
        // manual shape ([VARIANT], see the parser doc).
        None => None,
    };
    // `strictPort: Boolean(config.clientId)` (mcp-auth-flow.ts:507 @
    // 97435aab): a pre-registered client relying on the default path binds
    // the configured default port exactly.
    let pre_registered_default = config.client_id.is_some() && config.redirect_uri.is_none();
    let (callback_listener, actual_port) =
        bind_callback_endpoint(loopback.as_ref(), pre_registered_default).await?;
    let redirect_uri = match &config.redirect_uri {
        Some(uri) if uri.contains("{port}") => uri.replace("{port}", &actual_port.to_string()),
        Some(uri) => uri.clone(),
        None => format!("http://localhost:{actual_port}{DEFAULT_OAUTH_CALLBACK_PATH}"),
    };

    // Client identity ladder (#571 + #503 + stored-DCR reuse, ordered as
    // the upstream provider resolves it — mcp-oauth-provider.ts
    // clientInformation @ 97435aab): an explicit `clientId` wins; else a
    // stored DCR registration that is still REFRESH-CAPABLE (the DCR→CIMD
    // migration defers until the stored pair is invalidated — a refresh in
    // the transition window goes out with the registered client_id);
    // else CIMD when `oauth.clientMetadataUrl` is configured and the
    // discovered metadata advertises
    // `client_id_metadata_document_supported`; else the remaining stored
    // registration whose redirect_uris still cover the current callback
    // (#503: a stale redirect only drops the registration when the
    // credentials are not refresh-capable); else a fresh DCR.
    let mut client_secret = config.client_secret.clone();
    let client_id = match &config.client_id {
        Some(id) => id.clone(),
        None => {
            // `getAuthForUrl` scoping (mcp-auth-flow.ts:556 — the upstream
            // interactive leg reads the stored auth through the URL-matched
            // entry): a registration stored under a DIFFERENT server URL is
            // invisible here — neither reused against the new authorization
            // server nor destructively cleared when it looks dead.
            let stored_entry = store.get_for_url(server_name, server_url)?;
            let refresh_capable = stored_entry
                .as_ref()
                .and_then(|entry| entry.tokens.as_ref())
                .and_then(|tokens| tokens.refresh_token.as_ref())
                .is_some();
            let cimd_active = config.client_metadata_url.is_some()
                && metadata.client_id_metadata_document_supported == Some(true);
            let has_tokens = stored_entry
                .as_ref()
                .is_some_and(|entry| entry.tokens.is_some());
            let stored = stored_entry
                .clone()
                .and_then(|entry| entry.client_info)
                .filter(|info| {
                    registration_reusable(info, has_tokens, refresh_capable, &redirect_uri)
                });
            // #503 (mcp-auth-flow.ts:558-560/:568-571): a dead registration
            // (orphaned, or stale redirect on a non-refreshable pair) is
            // cleared from the STORE too, not just filtered out of this
            // resolution — the upstream interactive leg clears it up front
            // so the next registration starts clean. Without this the CIMD
            // branch below would keep resurrecting the dead registration
            // on every flow.
            if let Some(info) = stored_entry
                .as_ref()
                .and_then(|entry| entry.client_info.as_ref())
            {
                if !registration_reusable(info, has_tokens, refresh_capable, &redirect_uri) {
                    let _ = store.clear_client_info(server_name);
                }
            }
            if let Some(info) = &stored {
                if refresh_capable {
                    // The stored DCR pair still refreshes — it wins over
                    // CIMD until invalidation (review P2-1 fix; upstream
                    // "preserves a stored DCR refresh pair before
                    // transitioning to CIMD").
                    if client_secret.is_none() {
                        client_secret = info.client_secret.clone();
                    }
                    info.client_id.clone()
                } else if cimd_active {
                    // CIMD (#571): the operator-supplied URL IS the
                    // client_id; no secret accompanies it and DCR is
                    // skipped (any dead registration was already cleared
                    // above).
                    config.client_metadata_url.clone().unwrap_or_default()
                } else {
                    if client_secret.is_none() {
                        client_secret = info.client_secret.clone();
                    }
                    info.client_id.clone()
                }
            } else if cimd_active {
                config.client_metadata_url.clone().unwrap_or_default()
            } else {
                let (id, secret) =
                    register_client(&metadata, server_name, &redirect_uri, &fetch).await?;
                store.update_client_info(
                    server_name,
                    store::StoredClientInfo {
                        client_id: id.clone(),
                        client_secret: secret.clone(),
                        redirect_uris: Some(vec![redirect_uri.clone()]),
                        ..Default::default()
                    },
                    Some(server_url),
                )?;
                // DCR-issued secret authenticates the token exchange even
                // though it is not in the config (upstream stores it on
                // the provider and the SDK applies it automatically).
                if client_secret.is_none() {
                    client_secret = secret;
                }
                id
            }
        }
    };

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
        &fetch,
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
    fetch: &OAuthFetch,
) -> Result<StoredTokens, AdapterError> {
    let client = fetch.client(token_endpoint)?;

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
        .headers(fetch.request_headers(token_endpoint, &[("accept", "application/json")]))
        .form(&form)
        .send()
        .await
        .map_err(|e| fetch.map_send_error(token_endpoint, "token exchange", e))?;

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
    fetch: &OAuthFetch,
) -> Result<StoredTokens, AdapterError> {
    let client = fetch.client(token_endpoint)?;

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
        .headers(fetch.request_headers(token_endpoint, &[("accept", "application/json")]))
        .form(&form)
        .send()
        .await
        .map_err(|e| fetch.map_send_error(token_endpoint, "token refresh", e))?;

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

/// `parseOAuthRedirectUri` (mcp-auth-flow.ts:373-427 @ 97435aab, #483):
/// classify `oauth.redirectUri`. A `{port}` placeholder (at most one, in
/// the port position) takes an OS-assigned port; an explicit port binds
/// exactly; `https://` non-loopback is the MANUAL mode; everything else
/// is a HARD config error that fails the flow fast (upstream messages).
/// Returns `Ok(None)` for the manual mode — [VARIANT] rpi has no manual
/// completion input surface (`auth-complete` is not in scope), so the
/// default listener plus the 5-minute callback window is the existing
/// observable shape for manual flows.
fn parse_loopback_redirect_uri(
    redirect_uri: &str,
) -> Result<Option<(String, Option<u16>, String)>, AdapterError> {
    let invalid = |message: &str| AdapterError::InvalidConfigValue(message.to_string());
    let placeholder_count = redirect_uri.matches("{port}").count();
    if placeholder_count > 1 {
        return Err(invalid(
            "OAuth redirectUri may contain at most one {port} placeholder",
        ));
    }
    let dynamic_port = placeholder_count == 1;
    let parseable = if dynamic_port {
        let authority_start = match redirect_uri.find("://") {
            Some(offset) => offset + 3,
            None => {
                return Err(invalid(&format!(
                    "Invalid OAuth redirectUri: {redirect_uri}"
                )))
            }
        };
        let authority_end = redirect_uri[authority_start..]
            .find(['/', '?', '#'])
            .map(|offset| authority_start + offset)
            .unwrap_or(redirect_uri.len());
        let authority = &redirect_uri[authority_start..authority_end];
        if !authority.ends_with(":{port}") {
            return Err(invalid(
                "OAuth redirectUri {port} placeholder must be the loopback URI port",
            ));
        }
        redirect_uri.replace("{port}", "1")
    } else {
        redirect_uri.to_string()
    };
    let url = match url::Url::parse(&parseable) {
        Ok(url) => url,
        Err(_) => {
            return Err(invalid(&format!(
                "Invalid OAuth redirectUri: {redirect_uri}"
            )))
        }
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(
            "OAuth redirectUri must not include username or password",
        ));
    }
    if url.fragment().is_some() {
        return Err(invalid("OAuth redirectUri must not include a fragment"));
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let is_loopback =
        host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]";
    if dynamic_port && (url.scheme() != "http" || !is_loopback) {
        return Err(invalid(
            "OAuth redirectUri {port} placeholder is allowed only for an http:// localhost or loopback URI",
        ));
    }
    if let Some(port) = url.port() {
        if port == 0 {
            return Err(invalid(
                "OAuth redirectUri port must be a positive numeric port",
            ));
        }
    } else if let Some(raw_port) = url
        .authority()
        .rsplit_once(':')
        .map(|(_, raw)| raw)
        .filter(|raw| raw.chars().all(|c| c.is_ascii_digit()) && !raw.is_empty())
    {
        // A syntactically numeric but out-of-range port (WHATWG parsing
        // drops it from `port()`); upstream's Number.parseInt gate rejects
        // anything that is not a positive integer ≤ 65535.
        let _ = raw_port;
        return Err(invalid(
            "OAuth redirectUri port must be a positive numeric port",
        ));
    }
    if url.scheme() == "https" && !is_loopback {
        // Manual mode (see the [VARIANT] note above).
        return Ok(None);
    }
    if url.scheme() != "http" || !is_loopback {
        return Err(invalid(
            "OAuth redirectUri must be an https:// URI or an http:// localhost or loopback URI",
        ));
    }
    if url.port().is_none() && !dynamic_port {
        return Err(invalid(
            "OAuth localhost redirectUri must include an explicit numeric port",
        ));
    }
    let callback_host = if host == "[::1]" {
        "::1".to_string()
    } else {
        host
    };
    // Re-review Finding 1: a `{port}` URI parsed through the `:1` sentinel
    // must NOT leak the sentinel into the bind target — dynamic-port URIs
    // take an OS-assigned port (upstream `strictPort: !dynamicPort`).
    let port = if dynamic_port { None } else { url.port() };
    Ok(Some((callback_host, port, url.path().to_string())))
}

/// `ensureCallbackServer` (#483): bind the callback listener. With a
/// loopback redirect target the explicit port binds EXACTLY (strictPort);
/// a `{port}` placeholder or the dynamic-client default takes an
/// OS-assigned port. Without a redirect target, a pre-registered
/// (`clientId`) client binds the configured default port strictly and a
/// dynamic client takes an OS-assigned port.
async fn bind_callback_endpoint(
    loopback: Option<&(String, Option<u16>, String)>,
    pre_registered_default: bool,
) -> Result<(tokio::net::TcpListener, u16), AdapterError> {
    let (bind_host, strict_port): (&str, Option<u16>) = match loopback {
        Some((host, Some(port), _)) => (host.as_str(), Some(*port)),
        Some((host, None, _)) => (host.as_str(), None),
        None => (
            "127.0.0.1",
            pre_registered_default.then(configured_oauth_callback_port),
        ),
    };
    let bind_addr = format!(
        "{bind_host}:{}",
        strict_port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "0".to_string())
    );
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| {
            AdapterError::InvalidConfigValue(format!("callback server bind {bind_addr}: {e}"))
        })?;
    let port = listener
        .local_addr()
        .map_err(|e| AdapterError::InvalidConfigValue(format!("callback server addr: {e}")))?
        .port();
    tracing::debug!(
        port,
        host = bind_host,
        "MCP OAuth callback server listening"
    );
    Ok((listener, port))
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
    let fetch = OAuthFetch::new(definition, server_url, server_name);
    let metadata = match discover_auth_server_metadata_with_override(
        server_url,
        false,
        metadata_override.as_deref(),
        &fetch,
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
        &fetch,
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
    fn client_metadata_url_without_client_id_rejects_a_secret_with_upstream_text() {
        // mcp-auth-flow.ts:222-223 (review round 2: the message is the
        // upstream literal, not a paraphrase).
        let entry = ServerEntry(
            json!({
                "url": "https://test/mcp",
                "oauth": {
                    "clientMetadataUrl": "https://cimd.example.test/client.json",
                    "clientSecret": "secret"
                }
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        let config = parse_oauth_config(&entry);
        assert!(
            matches!(
                validate_oauth_config(&config),
                Err(AdapterError::InvalidConfigValue(message))
                    if message
                        == "OAuth clientSecret requires an explicit clientId when clientMetadataUrl is configured"
            ),
            "the CIMD + secret combination uses the upstream message"
        );
    }

    #[test]
    fn client_metadata_url_interpolates_env_and_trims() {
        // #571 (mcp-auth-flow.ts:213-219): the URL is env-interpolated and
        // trimmed at parse; an interpolation that empties it is rejected
        // at the auth boundary (an unset env var must not silently change
        // the CIMD surface).
        std::env::set_var("RPI_TEST_CIMD_HOST", "cimd.example.test");
        let entry = ServerEntry(
            json!({
                "url": "https://test/mcp",
                "oauth": {
                    "clientMetadataUrl": "  https://${RPI_TEST_CIMD_HOST}/client.json \n"
                }
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        let config = parse_oauth_config(&entry);
        assert_eq!(
            config.client_metadata_url.as_deref(),
            Some("https://cimd.example.test/client.json")
        );
        std::env::remove_var("RPI_TEST_CIMD_HOST");

        let unset = ServerEntry(
            json!({
                "url": "https://test/mcp",
                "oauth": { "clientMetadataUrl": "${RPI_TEST_CIMD_UNSET_HOST}" }
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        let config = parse_oauth_config(&unset);
        assert_eq!(config.client_metadata_url.as_deref(), Some(""));
        assert!(
            matches!(
                validate_oauth_config(&config),
                Err(AdapterError::InvalidConfigValue(message))
                    if message.contains("must not be empty")
            ),
            "an env-interpolated empty URL is rejected"
        );
    }

    #[test]
    fn registration_reusable_gate_matches_the_503_semantics() {
        // #503 (mcp-auth-flow.ts:556-573): orphaned registrations are dead;
        // with tokens, reuse only when the stored redirect_uris contain the
        // current callback or the pair is refresh-capable; an absent list
        // never matches.
        let info = |uris: Option<Vec<String>>| store::StoredClientInfo {
            client_id: "client-1".to_string(),
            redirect_uris: uris,
            ..Default::default()
        };
        let callback = "http://127.0.0.1:19876/callback";
        // Orphaned (no tokens): dead even with a matching redirect.
        assert!(!registration_reusable(
            &info(Some(vec![callback.to_string()])),
            false,
            true,
            callback
        ));
        // Tokens + matching redirect: reusable.
        assert!(registration_reusable(
            &info(Some(vec![callback.to_string()])),
            true,
            false,
            callback
        ));
        // Tokens + stale redirect but refresh-capable: reusable.
        assert!(registration_reusable(
            &info(Some(vec!["https://old.test/cb".to_string()])),
            true,
            true,
            callback
        ));
        // Tokens + stale redirect + not refresh-capable: dead (cleared).
        assert!(!registration_reusable(
            &info(Some(vec!["https://old.test/cb".to_string()])),
            true,
            false,
            callback
        ));
        // An absent redirect list never matches.
        assert!(!registration_reusable(&info(None), true, false, callback));
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
    /// #483 (review P2-2 fix): the redirect-URI classification ladder —
    /// hard errors fail fast with the upstream messages; only the
    /// https-non-loopback MANUAL mode maps to `None`.
    #[test]
    fn loopback_redirect_uri_error_ladder() {
        use super::parse_loopback_redirect_uri as parse;
        // Valid local forms.
        assert!(parse("http://localhost:3118/callback").is_ok());
        assert!(parse("http://[::1]:4321/cb").is_ok());
        // Re-review Finding 1: a {port} URI takes an OS-ASSIGNED port — the
        // :1 sentinel never reaches the bind target.
        let dynamic = parse("http://127.0.0.1:{port}/callback")
            .expect("dynamic port parses")
            .expect("local target");
        assert_eq!(dynamic.1, None, "{{port}} must bind OS-assigned, not :1");
        // Manual mode.
        assert!(parse("https://client.example.com/client.json")
            .expect("manual")
            .is_none());
        // Hard errors, message for message with upstream.
        let cases = [
            (
                "http://localhost:{port}:{port}/cb",
                "OAuth redirectUri may contain at most one {port} placeholder",
            ),
            (
                "http://localhost:8080{port}/cb",
                "OAuth redirectUri {port} placeholder must be the loopback URI port",
            ),
            (
                "http://localhost:8080/cb#frag",
                "OAuth redirectUri must not include a fragment",
            ),
            (
                "http://user:pass@localhost:8080/cb",
                "OAuth redirectUri must not include username or password",
            ),
            (
                "https://client.example.test:{port}/c.json",
                "OAuth redirectUri {port} placeholder is allowed only for an http:// localhost or loopback URI",
            ),
            (
                "http://localhost:0/cb",
                "OAuth redirectUri port must be a positive numeric port",
            ),
            (
                "http://localhost/cb",
                "OAuth localhost redirectUri must include an explicit numeric port",
            ),
            (
                "ftp://localhost:8080/cb",
                "OAuth redirectUri must be an https:// URI or an http:// localhost or loopback URI",
            ),
            // Re-review Finding 4: the generic parse-failure message (the
            // 9th upstream throw site).
            (
                "not a url at all",
                "Invalid OAuth redirectUri: not a url at all",
            ),
        ];
        for (uri, expected) in cases {
            let error = parse(uri).expect_err(uri);
            assert_eq!(
                error.to_string(),
                format!("invalid config value: {expected}"),
                "uri: {uri}"
            );
        }
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
