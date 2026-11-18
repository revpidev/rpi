//! OAuth 2.1 for MCP (port of `packages/mcp/src/oauth/` @ b4afdddf),
//! including the v1.0.0 hardening: `authServerMetadataUrl`, RFC 9207 `iss`,
//! step-up scope merging, credential-key migration support and null/empty
//! optional-field tolerance.

pub mod callback;
pub mod client_settings;
pub mod discovery;
pub mod errors;
pub mod flow;
pub mod provider;
pub mod sign_in;
pub mod types;

pub use callback::{
    OAuthCallback, OAuthCallbackPage, OAuthCallbackServer, OAuthCallbackServerOptions,
};
pub use client_settings::{
    McpOAuthClientOptions, McpOAuthSettings, create_oauth_client_provider, registered_redirect_urls,
};
pub use discovery::{
    AuthorizationServerMetadataOptions, OAuthServerInfoOptions, ProtectedResourceMetadataOptions,
    build_authorization_server_discovery_urls, discover_authorization_server_metadata,
    discover_oauth_server_info, discover_protected_resource_metadata, resource_url_from_server_url,
    select_resource,
};
pub use errors::OAuthFlowError;
pub use flow::{
    AdaptedOAuthProvider, CredentialKind, ExchangeAuthorizationCodeOptions,
    OAuthClientMetadataDocument, OAuthClientProvider, OAuthFlowOptions, OAuthFlowResult,
    RefreshAuthorizationOptions, RegisterClientOptions, StartAuthorizationOptions,
    TokenRequestOptions, adapt_oauth_provider, authorize_mcp, exchange_authorization_code,
    refresh_authorization, register_client, start_authorization, step_up_scope,
};
pub use provider::{
    ClientMetadataDocumentFn, McpOAuthProvider, McpOAuthProviderOptions, McpOAuthState,
    McpOAuthStateStore, McpRefreshLockGuard, MemoryOAuthStateStore,
};
pub use sign_in::{
    McpSignInOptions, McpSignInRedirect, McpSignInStart, complete_mcp_sign_in,
    refresh_stored_tokens, start_mcp_sign_in,
};
pub use types::{
    AuthorizationServerMetadata, OAuthChallenge, OAuthClientInformation, OAuthClientMetadata,
    OAuthDiscoveryState, OAuthProtectedResourceMetadata, OAuthServerInfo, OAuthTokens,
    parse_authorization_server_metadata, parse_client_information, parse_oauth_tokens,
    parse_protected_resource_metadata, parse_www_authenticate,
};
