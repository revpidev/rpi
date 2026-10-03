//! OAuth discovery (port of `packages/mcp/src/oauth/discovery.ts` @
//! a13d35a74): protected-resource metadata, authorization-server metadata,
//! `WWW-Authenticate` challenges and resource selection.

use serde_json::Value;
use url::Url;

use super::errors::OAuthFlowError;
use super::types::{
    AuthorizationServerMetadata, OAuthProtectedResourceMetadata, OAuthServerInfo,
    parse_authorization_server_metadata, parse_protected_resource_metadata,
};
use crate::types::LATEST_PROTOCOL_VERSION;

/// Options for [`discover_protected_resource_metadata`].
#[derive(Clone, Default)]
pub struct ProtectedResourceMetadataOptions {
    pub resource_metadata_url: Option<String>,
    pub protocol_version: Option<String>,
    pub client: Option<reqwest::Client>,
}

/// Options for [`discover_authorization_server_metadata`].
#[derive(Clone, Default)]
pub struct AuthorizationServerMetadataOptions {
    pub protocol_version: Option<String>,
    pub skip_issuer_validation: bool,
    pub client: Option<reqwest::Client>,
}

/// Options for [`discover_oauth_server_info`].
#[derive(Clone, Default)]
pub struct OAuthServerInfoOptions {
    pub resource_metadata_url: Option<Url>,
    /// Metadata document to use instead of discovery. It is trusted as
    /// configured, so its issuer is not checked.
    pub authorization_server_metadata_url: Option<Url>,
    pub client: Option<reqwest::Client>,
    pub skip_issuer_validation: bool,
}

/// 4xx and 502 mean "not here", so discovery tries the next candidate URL.
fn is_discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

/// Path suffix for `/.well-known/<kind><path>`; empty for the root path.
fn path_suffix(pathname: &str) -> &str {
    pathname.strip_suffix('/').unwrap_or(pathname)
}

async fn fetch_metadata(
    client: &reqwest::Client,
    url: &Url,
    protocol_version: &str,
) -> Result<reqwest::Response, OAuthFlowError> {
    client
        .get(url.clone())
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", protocol_version)
        .send()
        .await
        .map_err(|error| OAuthFlowError::Network(error.to_string()))
}

/// `discoverProtectedResourceMetadata` (discovery.ts:63).
pub async fn discover_protected_resource_metadata(
    server_url: &str,
    options: &ProtectedResourceMetadataOptions,
) -> Result<OAuthProtectedResourceMetadata, OAuthFlowError> {
    let server = Url::parse(server_url)
        .map_err(|_| OAuthFlowError::Invalid("Invalid server URL".to_owned()))?;
    let client = options.client.clone().unwrap_or_default();
    let version = options
        .protocol_version
        .clone()
        .unwrap_or_else(|| LATEST_PROTOCOL_VERSION.to_owned());
    let origin = server.origin().ascii_serialization();
    let mut response = if let Some(resource_metadata_url) = &options.resource_metadata_url {
        let url = Url::parse(resource_metadata_url)
            .map_err(|_| OAuthFlowError::Invalid("Invalid resource metadata URL".to_owned()))?;
        fetch_metadata(&client, &url, &version).await?
    } else {
        let url = Url::parse(&format!(
            "{origin}/.well-known/oauth-protected-resource{}",
            path_suffix(server.path())
        ))
        .map_err(|_| OAuthFlowError::Invalid("Invalid server URL".to_owned()))?;
        fetch_metadata(&client, &url, &version).await?
    };
    if options.resource_metadata_url.is_none()
        && server.path() != "/"
        && is_discovery_miss(response.status().as_u16())
    {
        let url = Url::parse(&format!("{origin}/.well-known/oauth-protected-resource"))
            .map_err(|_| OAuthFlowError::Invalid("Invalid server URL".to_owned()))?;
        response = fetch_metadata(&client, &url, &version).await?;
    }
    if !response.status().is_success() {
        return Err(OAuthFlowError::Invalid(format!(
            "HTTP {} loading OAuth protected resource metadata",
            response.status().as_u16()
        )));
    }
    let value: Value = response.json().await.map_err(|error| {
        OAuthFlowError::Invalid(format!(
            "Invalid OAuth protected resource metadata: {error}"
        ))
    })?;
    parse_protected_resource_metadata(&value)
}

/// `buildAuthorizationServerDiscoveryUrls` (discovery.ts:88).
pub fn build_authorization_server_discovery_urls(
    authorization_server_url: &Url,
) -> Vec<(Url, &'static str)> {
    let origin = authorization_server_url.origin().ascii_serialization();
    let path = path_suffix(authorization_server_url.path());
    let mut base = Url::parse(&origin).map_err(|_| ()).ok();
    let mut urls = Vec::new();
    if let Some(root) = base.take() {
        if let Ok(url) = root.join(&format!("/.well-known/oauth-authorization-server{path}")) {
            urls.push((url, "oauth"));
        }
        if let Ok(url) = root.join(&format!("/.well-known/openid-configuration{path}")) {
            urls.push((url, "oidc"));
        }
        if !path.is_empty()
            && let Ok(url) = root.join(&format!("{path}/.well-known/openid-configuration"))
        {
            urls.push((url, "oidc"));
        }
    }
    urls
}

/// `discoverAuthorizationServerMetadata` (discovery.ts:101).
pub async fn discover_authorization_server_metadata(
    authorization_server_url: &str,
    options: &AuthorizationServerMetadataOptions,
) -> Result<Option<AuthorizationServerMetadata>, OAuthFlowError> {
    let issuer = Url::parse(authorization_server_url)
        .map_err(|_| OAuthFlowError::Invalid("Invalid authorization server URL".to_owned()))?;
    let client = options.client.clone().unwrap_or_default();
    let version = options
        .protocol_version
        .clone()
        .unwrap_or_else(|| LATEST_PROTOCOL_VERSION.to_owned());
    for (url, _kind) in build_authorization_server_discovery_urls(&issuer) {
        let response = fetch_metadata(&client, &url, &version).await?;
        if !response.status().is_success() {
            if is_discovery_miss(response.status().as_u16()) {
                continue;
            }
            return Err(OAuthFlowError::Invalid(format!(
                "HTTP {} loading authorization server metadata from {url}",
                response.status().as_u16()
            )));
        }
        let value: Value = response.json().await.map_err(|error| {
            OAuthFlowError::Invalid(format!("Invalid authorization server metadata: {error}"))
        })?;
        let metadata = parse_authorization_server_metadata(&value)?;
        if !options.skip_issuer_validation {
            let expected = authorization_server_url.to_owned();
            let trim = |value: &str| value.strip_suffix('/').unwrap_or(value).to_owned();
            if trim(&metadata.issuer) != trim(&expected) {
                return Err(OAuthFlowError::IssuerMismatch {
                    expected,
                    received: Some(metadata.issuer),
                });
            }
        }
        return Ok(Some(metadata));
    }
    Ok(None)
}

/// `discoverOAuthServerInfo` (discovery.ts:125).
pub async fn discover_oauth_server_info(
    server_url: &str,
    options: &OAuthServerInfoOptions,
) -> Result<OAuthServerInfo, OAuthFlowError> {
    let mut resource_metadata: Option<OAuthProtectedResourceMetadata> = None;
    match discover_protected_resource_metadata(
        server_url,
        &ProtectedResourceMetadataOptions {
            resource_metadata_url: options.resource_metadata_url.as_ref().map(Url::to_string),
            client: options.client.clone(),
            protocol_version: None,
        },
    )
    .await
    {
        Ok(metadata) => resource_metadata = Some(metadata),
        Err(error) => {
            // Network failures propagate; invalid metadata falls back to the
            // server origin (v1.0.0 `8ce69e9d2`).
            if error.is_network() {
                return Err(error);
            }
        }
    }
    if let Some(metadata_url) = &options.authorization_server_metadata_url {
        let client = options.client.clone().unwrap_or_default();
        let response = fetch_metadata(&client, metadata_url, LATEST_PROTOCOL_VERSION).await?;
        if !response.status().is_success() {
            return Err(OAuthFlowError::Invalid(format!(
                "HTTP {} loading authorization server metadata from {metadata_url}",
                response.status().as_u16()
            )));
        }
        let value: Value = response.json().await.map_err(|error| {
            OAuthFlowError::Invalid(format!("Invalid authorization server metadata: {error}"))
        })?;
        let metadata = parse_authorization_server_metadata(&value)?;
        return Ok(OAuthServerInfo {
            authorization_server_url: metadata.issuer.clone(),
            authorization_server_metadata: Some(metadata),
            resource_metadata,
        });
    }
    let server = Url::parse(server_url)
        .map_err(|_| OAuthFlowError::Invalid("Invalid server URL".to_owned()))?;
    let authorization_server_url = resource_metadata
        .as_ref()
        .and_then(|metadata| metadata.authorization_servers.as_ref())
        .and_then(|servers| servers.first())
        .cloned()
        .unwrap_or_else(|| {
            let mut origin = server.origin().ascii_serialization();
            origin.push('/');
            origin
        });
    let metadata = discover_authorization_server_metadata(
        &authorization_server_url,
        &AuthorizationServerMetadataOptions {
            skip_issuer_validation: options.skip_issuer_validation,
            client: options.client.clone(),
            protocol_version: None,
        },
    )
    .await?;
    Ok(OAuthServerInfo {
        authorization_server_url,
        authorization_server_metadata: metadata,
        resource_metadata,
    })
}

/// `resourceUrlFromServerUrl` (discovery.ts:151): the URL without its
/// fragment.
pub fn resource_url_from_server_url(value: &str) -> Result<Url, OAuthFlowError> {
    let mut url =
        Url::parse(value).map_err(|_| OAuthFlowError::Invalid("Invalid server URL".to_owned()))?;
    url.set_fragment(None);
    Ok(url)
}

/// `selectResource` (discovery.ts:157): the resource a protected-resource
/// metadata document names, when it matches the MCP server.
pub fn select_resource(
    server_url: &str,
    metadata: Option<&OAuthProtectedResourceMetadata>,
) -> Result<Option<String>, OAuthFlowError> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let requested = resource_url_from_server_url(server_url)?;
    let configured = Url::parse(&metadata.resource)
        .map_err(|_| OAuthFlowError::Invalid("Invalid resource URL".to_owned()))?;
    if requested.origin() != configured.origin() {
        return Err(OAuthFlowError::Invalid(format!(
            "Protected resource {} does not match MCP server {requested}",
            metadata.resource
        )));
    }
    let requested_path = if requested.path().ends_with('/') {
        requested.path().to_owned()
    } else {
        format!("{}/", requested.path())
    };
    let configured_path = if configured.path().ends_with('/') {
        configured.path().to_owned()
    } else {
        format!("{}/", configured.path())
    };
    if !requested_path.starts_with(&configured_path) {
        return Err(OAuthFlowError::Invalid(format!(
            "Protected resource {} does not match MCP server {requested}",
            metadata.resource
        )));
    }
    Ok(Some(metadata.resource.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_urls_match_upstream_shapes() {
        let root = Url::parse("https://as.example").unwrap();
        let urls = build_authorization_server_discovery_urls(&root);
        assert_eq!(
            urls.iter().map(|(url, _)| url.as_str()).collect::<Vec<_>>(),
            vec![
                "https://as.example/.well-known/oauth-authorization-server",
                "https://as.example/.well-known/openid-configuration",
            ]
        );

        let path = Url::parse("https://as.example/tenant/").unwrap();
        let urls = build_authorization_server_discovery_urls(&path);
        assert_eq!(
            urls.iter().map(|(url, _)| url.as_str()).collect::<Vec<_>>(),
            vec![
                "https://as.example/.well-known/oauth-authorization-server/tenant",
                "https://as.example/.well-known/openid-configuration/tenant",
                "https://as.example/tenant/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn select_resource_requires_same_origin_and_path_prefix() {
        let metadata = OAuthProtectedResourceMetadata {
            resource: "https://mcp.example/api".to_owned(),
            authorization_servers: None,
            scopes_supported: None,
            extra: Default::default(),
        };
        assert_eq!(
            select_resource("https://mcp.example/api/mcp", Some(&metadata)).unwrap(),
            Some("https://mcp.example/api".to_owned())
        );
        assert!(select_resource("https://other.example/api", Some(&metadata)).is_err());
        assert!(select_resource("https://mcp.example/other", Some(&metadata)).is_err());
        assert_eq!(select_resource("https://mcp.example", None).unwrap(), None);
    }
}
