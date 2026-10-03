//! OAuth authorization code flow with PKCE and dynamic client
//! registration (port of `packages/mcp/src/oauth/flow.ts` @ a13d35a74).
//!
//! Includes the v1.0.0 hardening: `authServerMetadataUrl` replaces
//! discovery (trusted as configured), RFC 9207 `iss` is validated before a
//! code is exchanged, `insufficient_scope` step-up merges the granted scope,
//! and a refresh without `scope` keeps the grant's scope.

use std::sync::Arc;

use base64::Engine;
use serde_json::{Value, json};
use url::Url;

use super::discovery::{
    AuthorizationServerMetadataOptions, OAuthServerInfoOptions,
    discover_authorization_server_metadata, discover_oauth_server_info, select_resource,
};
use super::errors::OAuthFlowError;
use super::types::{
    AuthorizationServerMetadata, OAuthChallenge, OAuthClientInformation, OAuthClientMetadata,
    OAuthDiscoveryState, OAuthServerInfo, OAuthTokens, parse_client_information,
    parse_oauth_tokens, parse_www_authenticate,
};
use crate::auth_provider::{AuthProvider, UnauthorizedContext};
use crate::protocol::McpError;

/// Credentials buckets `invalidateCredentials` can drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    All,
    Client,
    Tokens,
    Verifier,
    Discovery,
}

/// `OAuthClientProvider` (flow.ts:37): the stateful seam the flow drives.
#[async_trait::async_trait]
pub trait OAuthClientProvider: Send + Sync {
    /// `redirectUrl` (flow.ts:38).
    fn redirect_url(&self) -> String;
    /// `clientMetadata` (flow.ts:39).
    fn client_metadata(&self) -> OAuthClientMetadata;
    /// `clientMetadataUrl` (flow.ts:40): the URL used as `client_id` when
    /// the authorization server supports metadata documents.
    fn client_metadata_url(&self) -> Option<String> {
        None
    }
    /// `state?()` (flow.ts:41).
    async fn state(&self) -> Option<String> {
        None
    }
    /// `clientInformation()` (flow.ts:42).
    async fn client_information(&self) -> Option<OAuthClientInformation>;
    /// `saveClientInformation?()` (flow.ts:43).
    async fn save_client_information(&self, information: OAuthClientInformation);
    /// `tokens()` (flow.ts:44).
    async fn tokens(&self) -> Option<OAuthTokens>;
    /// `saveTokens()` (flow.ts:45).
    async fn save_tokens(&self, tokens: OAuthTokens);
    /// `redirectToAuthorization()` (flow.ts:46).
    async fn redirect_to_authorization(&self, url: Url);
    /// `saveCodeVerifier()` (flow.ts:47).
    async fn save_code_verifier(&self, verifier: String);
    /// `codeVerifier()` (flow.ts:48).
    async fn code_verifier(&self) -> Result<String, OAuthFlowError>;
    /// `invalidateCredentials?()` (flow.ts:50).
    async fn invalidate_credentials(&self, _kind: CredentialKind) {}
    /// `saveDiscoveryState?()` (flow.ts:51).
    async fn save_discovery_state(&self, _state: OAuthDiscoveryState) {}
    /// `discoveryState?()` (flow.ts:52).
    async fn discovery_state(&self) -> Option<OAuthDiscoveryState> {
        None
    }
}

/// `OAuthFlowOptions` (flow.ts:74).
#[derive(Clone, Default)]
pub struct OAuthFlowOptions {
    pub server_url: String,
    pub authorization_code: Option<String>,
    /// `iss` parameter of the authorization response (RFC 9207).
    pub iss: Option<String>,
    pub scope: Option<String>,
    pub resource_metadata_url: Option<Url>,
    /// Authorization server metadata document to use instead of discovery.
    /// Trusted as configured; must use https, except on loopback.
    pub authorization_server_metadata_url: Option<Url>,
    pub client: Option<reqwest::Client>,
    pub skip_issuer_validation: bool,
    /// Go straight to the authorization redirect instead of refreshing
    /// stored tokens (a refresh keeps the old scope).
    pub skip_refresh: bool,
}

/// `OAuthFlowResult` (flow.ts:99).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthFlowResult {
    Authorized,
    Redirect,
}

/// `onChallenge` listener (oauth.ts host seam).
pub type ChallengeListener = Arc<dyn Fn(&OAuthChallenge) + Send + Sync>;

type ClientAuthMethod = &'static str;

const CLIENT_SECRET_BASIC: ClientAuthMethod = "client_secret_basic";
const CLIENT_SECRET_POST: ClientAuthMethod = "client_secret_post";
const NONE: ClientAuthMethod = "none";

fn loopback(hostname: &str) -> bool {
    matches!(hostname, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// `secureEndpoint` (flow.ts:118).
fn secure_endpoint(value: &str) -> Result<Url, OAuthFlowError> {
    let url = Url::parse(value)
        .map_err(|_| OAuthFlowError::Invalid(format!("Invalid endpoint URL {value}")))?;
    if url.scheme() != "https" && !loopback(url.host_str().unwrap_or_default()) {
        return Err(OAuthFlowError::InsecureEndpoint {
            endpoint: url.to_string(),
        });
    }
    Ok(url)
}

/// `selectClientAuthMethod` (flow.ts:124).
fn select_client_auth_method(
    information: &OAuthClientInformation,
    supported: &[String],
) -> ClientAuthMethod {
    let hinted = information
        .extra
        .get("token_endpoint_auth_method")
        .and_then(Value::as_str)
        .or(information
            .extra
            .get("token_endpoint_auth_method")
            .and_then(Value::as_str));
    if let Some(hinted) = hinted
        && matches!(hinted, CLIENT_SECRET_BASIC | CLIENT_SECRET_POST | NONE)
        && (supported.is_empty() || supported.iter().any(|method| method == hinted))
    {
        return match hinted {
            CLIENT_SECRET_BASIC => CLIENT_SECRET_BASIC,
            CLIENT_SECRET_POST => CLIENT_SECRET_POST,
            _ => NONE,
        };
    }
    let has_secret = information
        .client_secret
        .as_ref()
        .is_some_and(|secret| !secret.is_empty());
    if supported.is_empty() {
        return if has_secret {
            CLIENT_SECRET_BASIC
        } else {
            NONE
        };
    }
    if has_secret && supported.iter().any(|method| method == CLIENT_SECRET_BASIC) {
        return CLIENT_SECRET_BASIC;
    }
    if has_secret && supported.iter().any(|method| method == CLIENT_SECRET_POST) {
        return CLIENT_SECRET_POST;
    }
    if supported.iter().any(|method| method == NONE) {
        return NONE;
    }
    if has_secret { CLIENT_SECRET_POST } else { NONE }
}

/// `applyClientAuthentication` (flow.ts:140).
fn apply_client_authentication(
    method: ClientAuthMethod,
    information: &OAuthClientInformation,
    headers: &mut reqwest::header::HeaderMap,
    params: &mut Vec<(String, String)>,
) -> Result<(), OAuthFlowError> {
    if method == CLIENT_SECRET_BASIC {
        let Some(secret) = information
            .client_secret
            .as_ref()
            .filter(|secret| !secret.is_empty())
        else {
            return Err(OAuthFlowError::Invalid(
                "client_secret_basic requires a client secret".to_owned(),
            ));
        };
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{secret}", information.client_id));
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Basic {encoded}"))
                .map_err(|_| OAuthFlowError::Invalid("Invalid client credentials".to_owned()))?,
        );
    } else {
        params.push(("client_id".to_owned(), information.client_id.clone()));
        if method == CLIENT_SECRET_POST
            && let Some(secret) = information
                .client_secret
                .as_ref()
                .filter(|secret| !secret.is_empty())
        {
            params.push(("client_secret".to_owned(), secret.clone()));
        }
    }
    Ok(())
}

/// `pkce` (flow.ts:157): 32 random bytes, base64url without padding.
fn pkce() -> (String, String) {
    let (challenge, verifier) = oauth2::PkceCodeChallenge::new_random_sha256();
    (verifier.secret().to_owned(), challenge.as_str().to_owned())
}

/// `startAuthorization` (flow.ts:165).
pub fn start_authorization(
    authorization_server_url: &str,
    options: &StartAuthorizationOptions,
) -> Result<(Url, String), OAuthFlowError> {
    let metadata = options.metadata.as_ref();
    if let Some(metadata) = metadata
        && !metadata
            .response_types_supported
            .iter()
            .any(|value| value == "code")
    {
        return Err(OAuthFlowError::Invalid(
            "Authorization server does not support authorization codes".to_owned(),
        ));
    }
    if let Some(methods) =
        metadata.and_then(|metadata| metadata.code_challenge_methods_supported.as_ref())
        && !methods.iter().any(|method| method == "S256")
    {
        return Err(OAuthFlowError::Invalid(
            "Authorization server does not support PKCE S256".to_owned(),
        ));
    }
    let base = Url::parse(authorization_server_url)
        .map_err(|_| OAuthFlowError::Invalid("Invalid authorization server URL".to_owned()))?;
    let mut url = match metadata.map(|metadata| metadata.authorization_endpoint.clone()) {
        Some(endpoint) => Url::parse(&endpoint)
            .map_err(|_| OAuthFlowError::Invalid("Invalid authorization endpoint".to_owned()))?,
        None => base
            .join("/authorize")
            .map_err(|_| OAuthFlowError::Invalid("Invalid authorization server URL".to_owned()))?,
    };
    let (verifier, challenge) = pkce();
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &options.client_information.client_id);
        query.append_pair("code_challenge", &challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("redirect_uri", &options.redirect_url);
        if let Some(state) = &options.state {
            query.append_pair("state", state);
        }
        if let Some(scope) = &options.scope {
            query.append_pair("scope", scope);
        }
        if options.scope.as_deref().is_some_and(|scope| {
            scope
                .split_whitespace()
                .any(|part| part == "offline_access")
        }) {
            query.append_pair("prompt", "consent");
        }
        if let Some(resource) = &options.resource {
            query.append_pair("resource", resource);
        }
    }
    Ok((url, verifier))
}

/// `StartAuthorizationOptions` (flow.ts:167).
#[derive(Clone, Default)]
pub struct StartAuthorizationOptions {
    pub metadata: Option<AuthorizationServerMetadata>,
    pub client_information: OAuthClientInformation,
    pub redirect_url: String,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub resource: Option<String>,
}

/// `TokenRequestOptions` (flow.ts:104).
#[derive(Clone)]
pub struct TokenRequestOptions {
    pub metadata: Option<AuthorizationServerMetadata>,
    pub client_information: OAuthClientInformation,
    pub resource: Option<String>,
    pub client: Option<reqwest::Client>,
}

/// `tokenRequest` (flow.ts:191).
async fn token_request(
    authorization_server_url: &str,
    options: &TokenRequestOptions,
    mut params: Vec<(String, String)>,
) -> Result<OAuthTokens, OAuthFlowError> {
    let base = Url::parse(authorization_server_url)
        .map_err(|_| OAuthFlowError::Invalid("Invalid authorization server URL".to_owned()))?;
    let url = secure_endpoint(
        options
            .metadata
            .as_ref()
            .map(|metadata| metadata.token_endpoint.clone())
            .unwrap_or_else(|| {
                base.join("/token")
                    .map(|url| url.to_string())
                    .unwrap_or_default()
            })
            .as_str(),
    )?;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Accept",
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    if let Some(resource) = &options.resource {
        params.push(("resource".to_owned(), resource.clone()));
    }
    let method = select_client_auth_method(
        &options.client_information,
        options
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.token_endpoint_auth_methods_supported.as_deref())
            .unwrap_or_default(),
    );
    apply_client_authentication(
        method,
        &options.client_information,
        &mut headers,
        &mut params,
    )?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params)
        .finish();
    let client = options.client.clone().unwrap_or_default();
    let response = client
        .post(url.clone())
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|error| OAuthFlowError::Network(error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| OAuthFlowError::Network(error.to_string()))?;
    let value: Option<Value> = serde_json::from_str(&text).ok();
    // Servers may report OAuth errors with any status, so check the body
    // before the status.
    if let Some(value) = &value
        && let Some(map) = value.as_object()
        && let Some(error) = map.get("error").and_then(Value::as_str)
    {
        return Err(OAuthFlowError::OAuth {
            code: error.to_owned(),
            message: map
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or(error)
                .to_owned(),
            error_uri: map
                .get("error_uri")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    if !(200..300).contains(&status) {
        return Err(OAuthFlowError::OAuth {
            code: "server_error".to_owned(),
            message: format!("HTTP {status}: {text}"),
            error_uri: None,
        });
    }
    parse_oauth_tokens(value.as_ref().unwrap_or(&Value::Null))
}

/// `registerClient` (flow.ts:221).
pub async fn register_client(
    authorization_server_url: &str,
    options: &RegisterClientOptions,
) -> Result<OAuthClientInformation, OAuthFlowError> {
    let base = Url::parse(authorization_server_url)
        .map_err(|_| OAuthFlowError::Invalid("Invalid authorization server URL".to_owned()))?;
    let endpoint = match options.metadata.as_ref() {
        Some(metadata) => match &metadata.registration_endpoint {
            Some(endpoint) => Url::parse(endpoint)
                .map_err(|_| OAuthFlowError::Invalid("Invalid registration endpoint".to_owned()))?,
            None => {
                return Err(OAuthFlowError::Invalid(
                    "Authorization server does not support dynamic client registration".to_owned(),
                ));
            }
        },
        None => base
            .join("/register")
            .map_err(|_| OAuthFlowError::Invalid("Invalid authorization server URL".to_owned()))?,
    };
    let mut body = serde_json::to_value(&options.client_metadata).unwrap_or_else(|_| json!({}));
    if let Some(scope) = &options.scope
        && let Some(map) = body.as_object_mut()
    {
        map.insert("scope".to_owned(), Value::String(scope.clone()));
    }
    let client = options.client.clone().unwrap_or_default();
    let response = client
        .post(endpoint)
        .header("Accept", "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_string(&body).unwrap_or_default())
        .send()
        .await
        .map_err(|error| OAuthFlowError::Network(error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| OAuthFlowError::Network(error.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(OAuthFlowError::Registration { status, body: text });
    }
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        OAuthFlowError::Invalid(format!(
            "Invalid OAuth client registration response: {error}"
        ))
    })?;
    parse_client_information(&value)
}

/// `RegisterClientOptions` (flow.ts:223).
#[derive(Clone, Default)]
pub struct RegisterClientOptions {
    pub metadata: Option<AuthorizationServerMetadata>,
    pub client_metadata: OAuthClientMetadata,
    pub scope: Option<String>,
    pub client: Option<reqwest::Client>,
}

/// `exchangeAuthorizationCode` (flow.ts:247).
pub async fn exchange_authorization_code(
    authorization_server_url: &str,
    options: &ExchangeAuthorizationCodeOptions,
) -> Result<OAuthTokens, OAuthFlowError> {
    token_request(
        authorization_server_url,
        &options.token,
        vec![
            ("grant_type".to_owned(), "authorization_code".to_owned()),
            ("code".to_owned(), options.code.clone()),
            ("code_verifier".to_owned(), options.code_verifier.clone()),
            ("redirect_uri".to_owned(), options.redirect_url.clone()),
        ],
    )
    .await
}

/// `ExchangeAuthorizationCodeOptions` (flow.ts:248).
#[derive(Clone)]
pub struct ExchangeAuthorizationCodeOptions {
    pub token: TokenRequestOptions,
    pub code: String,
    pub code_verifier: String,
    pub redirect_url: String,
}

/// `refreshAuthorization` (flow.ts:261).
pub async fn refresh_authorization(
    authorization_server_url: &str,
    options: &RefreshAuthorizationOptions,
) -> Result<OAuthTokens, OAuthFlowError> {
    let tokens = token_request(
        authorization_server_url,
        &options.token,
        vec![
            ("grant_type".to_owned(), "refresh_token".to_owned()),
            ("refresh_token".to_owned(), options.refresh_token.clone()),
        ],
    )
    .await?;
    // A response without a new refresh token keeps the old one.
    let mut merged = OAuthTokens {
        refresh_token: Some(options.refresh_token.clone()),
        ..tokens
    };
    if merged.refresh_token.as_deref() == Some("") {
        merged.refresh_token = Some(options.refresh_token.clone());
    }
    Ok(merged)
}

/// `RefreshAuthorizationOptions` (flow.ts:262).
#[derive(Clone)]
pub struct RefreshAuthorizationOptions {
    pub token: TokenRequestOptions,
    pub refresh_token: String,
}

/// `withScope` (flow.ts:277): a response without `scope` grants the
/// requested scope (RFC 6749 §5.1/§6).
fn with_scope(mut tokens: OAuthTokens, scope: Option<&str>) -> OAuthTokens {
    if tokens.scope.is_none()
        && let Some(scope) = scope
    {
        tokens.scope = Some(scope.to_owned());
    }
    tokens
}

/// `stepUpScope` (flow.ts:287): the challenged scopes plus the granted ones
/// (SEP-2350).
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
    let challenged = challenged?;
    let mut scopes: Vec<String> = Vec::new();
    for scope in [granted, Some(challenged)].into_iter().flatten() {
        for part in scope.split_whitespace().filter(|part| !part.is_empty()) {
            if !scopes.iter().any(|existing| existing == part) {
                scopes.push(part.to_owned());
            }
        }
    }
    Some(scopes.join(" "))
}

/// `runFlow` (flow.ts:295).
async fn run_flow(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
) -> Result<OAuthFlowResult, OAuthFlowError> {
    let metadata_url = match &options.authorization_server_metadata_url {
        Some(url) => Some(secure_endpoint(url.as_str())?),
        None => None,
    };
    // With a configured metadata URL, discovery is not cached, so changing
    // the URL applies at once.
    let cached = if metadata_url.is_some() {
        None
    } else {
        provider.discovery_state().await
    };
    let discovered = match cached {
        Some(state) if !state.authorization_server_url.is_empty() => {
            let metadata = match state.authorization_server_metadata {
                Some(metadata) => Some(metadata),
                None => {
                    discover_authorization_server_metadata(
                        &state.authorization_server_url,
                        &AuthorizationServerMetadataOptions {
                            client: options.client.clone(),
                            skip_issuer_validation: options.skip_issuer_validation,
                            protocol_version: None,
                        },
                    )
                    .await?
                }
            };
            OAuthServerInfo {
                authorization_server_url: state.authorization_server_url,
                authorization_server_metadata: metadata,
                resource_metadata: state.resource_metadata,
            }
        }
        _ => {
            discover_oauth_server_info(
                &options.server_url,
                &OAuthServerInfoOptions {
                    resource_metadata_url: options.resource_metadata_url.clone(),
                    authorization_server_metadata_url: metadata_url.clone(),
                    client: options.client.clone(),
                    skip_issuer_validation: options.skip_issuer_validation,
                },
            )
            .await?
        }
    };
    if metadata_url.is_none() {
        provider
            .save_discovery_state(OAuthDiscoveryState {
                authorization_server_url: discovered.authorization_server_url.clone(),
                authorization_server_metadata: discovered.authorization_server_metadata.clone(),
                resource_metadata: discovered.resource_metadata.clone(),
                resource_metadata_url: options.resource_metadata_url.as_ref().map(Url::to_string),
            })
            .await;
    }
    let metadata = discovered.authorization_server_metadata.clone();
    let resource = select_resource(&options.server_url, discovered.resource_metadata.as_ref())?;
    let scope = options
        .scope
        .clone()
        .filter(|scope| !scope.is_empty())
        .or_else(|| {
            discovered
                .resource_metadata
                .as_ref()
                .and_then(|metadata| metadata.scopes_supported.as_ref())
                .map(|scopes| scopes.join(" "))
                .filter(|scope| !scope.is_empty())
        })
        .or_else(|| {
            provider
                .client_metadata()
                .scope
                .clone()
                .filter(|scope| !scope.is_empty())
        });
    let mut client = provider.client_information().await;
    if client.is_none() {
        if options.authorization_code.is_some() {
            return Err(OAuthFlowError::Invalid(
                "OAuth client information is missing during code exchange".to_owned(),
            ));
        }
        if metadata
            .as_ref()
            .and_then(|metadata| metadata.client_id_metadata_document_supported)
            .unwrap_or(false)
            && let Some(metadata_url) = provider.client_metadata_url()
        {
            let url = Url::parse(&metadata_url).map_err(|_| {
                OAuthFlowError::Invalid("Invalid OAuth client metadata URL".to_owned())
            })?;
            if url.scheme() != "https" || url.path() == "/" {
                return Err(OAuthFlowError::Invalid(
                    "Invalid OAuth client metadata URL".to_owned(),
                ));
            }
            let information = OAuthClientInformation {
                client_id: metadata_url,
                ..Default::default()
            };
            provider.save_client_information(information.clone()).await;
            client = Some(information);
        } else {
            let information = register_client(
                &discovered.authorization_server_url,
                &RegisterClientOptions {
                    metadata: metadata.clone(),
                    client_metadata: provider.client_metadata(),
                    scope: scope.clone(),
                    client: options.client.clone(),
                },
            )
            .await?;
            provider.save_client_information(information.clone()).await;
            client = Some(information);
        }
    }
    let client = client.expect("client information is set above");
    let token_options = TokenRequestOptions {
        metadata: metadata.clone(),
        client_information: client.clone(),
        resource: resource.clone(),
        client: options.client.clone(),
    };
    if let Some(code) = &options.authorization_code {
        // RFC 9207: never send a code from another authorization server to
        // this one.
        if let Some(metadata) = &metadata {
            let iss = options.iss.as_deref();
            if (iss.is_some()
                || metadata
                    .authorization_response_iss_parameter_supported
                    .unwrap_or(false))
                && iss != Some(metadata.issuer.as_str())
            {
                return Err(OAuthFlowError::IssuerMismatch {
                    expected: metadata.issuer.clone(),
                    received: iss.map(str::to_owned),
                });
            }
        }
        let tokens = exchange_authorization_code(
            &discovered.authorization_server_url,
            &ExchangeAuthorizationCodeOptions {
                token: token_options,
                code: code.clone(),
                code_verifier: provider.code_verifier().await?,
                redirect_url: provider.redirect_url(),
            },
        )
        .await?;
        provider
            .save_tokens(with_scope(tokens, scope.as_deref()))
            .await;
        return Ok(OAuthFlowResult::Authorized);
    }
    let existing = if options.skip_refresh {
        None
    } else {
        provider.tokens().await
    };
    if let Some(existing) = existing
        && let Some(refresh_token) = existing
            .refresh_token
            .clone()
            .filter(|token| !token.is_empty())
    {
        match refresh_authorization(
            &discovered.authorization_server_url,
            &RefreshAuthorizationOptions {
                token: token_options,
                refresh_token,
            },
        )
        .await
        {
            Ok(tokens) => {
                provider
                    .save_tokens(with_scope(tokens, existing.scope.as_deref()))
                    .await;
                return Ok(OAuthFlowResult::Authorized);
            }
            Err(error) => match &error {
                OAuthFlowError::InsecureEndpoint { .. } => return Err(error),
                OAuthFlowError::OAuth { code, .. } if code != "server_error" => return Err(error),
                _ => {}
            },
        }
    }
    let state = provider.state().await;
    let (authorization_url, code_verifier) = start_authorization(
        &discovered.authorization_server_url,
        &StartAuthorizationOptions {
            metadata,
            client_information: client,
            redirect_url: provider.redirect_url(),
            scope,
            state,
            resource,
        },
    )?;
    provider.save_code_verifier(code_verifier).await;
    provider.redirect_to_authorization(authorization_url).await;
    Ok(OAuthFlowResult::Redirect)
}

/// `authorizeMcp` (flow.ts:402): retries once after dropping the credentials
/// the server rejected.
pub async fn authorize_mcp(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
) -> Result<OAuthFlowResult, OAuthFlowError> {
    match run_flow(provider, options).await {
        Ok(result) => Ok(result),
        Err(error) => {
            if let Some(code) = error.oauth_code() {
                if code == "invalid_client" || code == "unauthorized_client" {
                    provider.invalidate_credentials(CredentialKind::All).await;
                    return run_flow(provider, options).await;
                }
                if code == "invalid_grant" {
                    provider
                        .invalidate_credentials(CredentialKind::Tokens)
                        .await;
                    return run_flow(provider, options).await;
                }
            }
            Err(error)
        }
    }
}

/// `adaptOAuthProvider` (flow.ts:420): an [`AuthProvider`] that refreshes
/// stored tokens after a 401, or reports that the user has to authorize
/// again. Concurrent 401s share one refresh (serialized by a lock), and a
/// request whose token was already replaced is just retried.
pub struct AdaptedOAuthProvider {
    provider: Arc<dyn OAuthClientProvider>,
    server_url: Url,
    client: Option<reqwest::Client>,
    refresh_lock: tokio::sync::Mutex<()>,
    on_challenge: Option<ChallengeListener>,
}

impl AdaptedOAuthProvider {
    pub fn new(provider: Arc<dyn OAuthClientProvider>, server_url: Url) -> Self {
        Self {
            provider,
            server_url,
            client: None,
            refresh_lock: tokio::sync::Mutex::new(()),
            on_challenge: None,
        }
    }

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    pub fn with_challenge_listener(
        mut self,
        listener: Arc<dyn Fn(&OAuthChallenge) + Send + Sync>,
    ) -> Self {
        self.on_challenge = Some(listener);
        self
    }
}

#[async_trait::async_trait]
impl AuthProvider for AdaptedOAuthProvider {
    async fn token(&self) -> Option<String> {
        self.provider
            .tokens()
            .await
            .map(|tokens| tokens.access_token)
    }

    async fn on_unauthorized(&self, context: UnauthorizedContext) -> Result<(), McpError> {
        let challenge = parse_www_authenticate(context.www_authenticate.as_deref());
        if let Some(listener) = &self.on_challenge {
            listener(&challenge);
        }
        let insufficient_scope = challenge.error.as_deref() == Some("insufficient_scope");
        if insufficient_scope {
            // A refresh keeps the granted scope, so more scope needs a new
            // sign-in.
            return Err(McpError::AuthorizationRequired);
        }
        let _guard = self.refresh_lock.lock().await;
        // A token that changed meanwhile (another request refreshed it, or
        // the user signed in) is used as is.
        if let Some(stale) = &context.token
            && let Some(current) = self
                .provider
                .tokens()
                .await
                .map(|tokens| tokens.access_token)
            && &current != stale
        {
            return Ok(());
        }
        let granted = self.provider.tokens().await;
        let scope = match step_up_scope(
            granted.as_ref().and_then(|tokens| tokens.scope.as_deref()),
            challenge.scope.as_deref(),
        ) {
            Some(scope) => Some(scope),
            None => challenge.scope.clone(),
        };
        let result = authorize_mcp(
            self.provider.as_ref(),
            &OAuthFlowOptions {
                server_url: self.server_url.to_string(),
                resource_metadata_url: challenge
                    .resource_metadata_url
                    .as_deref()
                    .and_then(|url| Url::parse(url).ok()),
                scope,
                client: self.client.clone(),
                ..Default::default()
            },
        )
        .await;
        match result {
            Ok(OAuthFlowResult::Authorized) => Ok(()),
            Ok(OAuthFlowResult::Redirect) => Err(McpError::AuthorizationRequired),
            Err(OAuthFlowError::AuthorizationRequired) => Err(McpError::AuthorizationRequired),
            Err(error) => Err(McpError::Transport(error.to_string())),
        }
    }
}

/// Convenience: wrap a provider as a transport [`AuthProvider`].
pub fn adapt_oauth_provider(
    provider: Arc<dyn OAuthClientProvider>,
    server_url: Url,
) -> Arc<dyn AuthProvider> {
    Arc::new(AdaptedOAuthProvider::new(provider, server_url))
}
