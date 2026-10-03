//! OAuth errors (port of `packages/mcp/src/oauth/errors.ts` @ a13d35a74).

/// `OAuthError` (errors.ts:1): an OAuth error response (`error` +
/// `error_description`), or a generic flow failure.
#[derive(Debug, Clone, thiserror::Error)]
pub enum OAuthFlowError {
    /// A server-reported OAuth error; `code` drives the retry policy.
    #[error("{message}")]
    OAuth {
        code: String,
        message: String,
        error_uri: Option<String>,
    },
    /// `OAuthIssuerMismatchError` (errors.ts:12).
    #[error("OAuth issuer mismatch: expected {expected:?}, received {received:?}")]
    IssuerMismatch {
        expected: String,
        received: Option<String>,
    },
    /// `OAuthInsecureEndpointError` (errors.ts:28).
    #[error("Refusing to send OAuth credentials to non-HTTPS endpoint {endpoint}")]
    InsecureEndpoint { endpoint: String },
    /// `OAuthRegistrationError` (errors.ts:37).
    #[error("OAuth dynamic client registration failed with status {status}: {body}")]
    Registration { status: u16, body: String },
    /// `McpOAuthAuthorizationRequiredError` (errors.ts:47).
    #[error("MCP OAuth authorization requires user interaction")]
    AuthorizationRequired,
    /// Generic invalid-data failures (upstream `Error`).
    #[error("{0}")]
    Invalid(String),
    /// Network failures (upstream `TypeError` from fetch).
    #[error("{0}")]
    Network(String),
}

impl OAuthFlowError {
    pub fn oauth_code(&self) -> Option<&str> {
        match self {
            OAuthFlowError::OAuth { code, .. } => Some(code),
            _ => None,
        }
    }

    pub fn is_network(&self) -> bool {
        matches!(self, OAuthFlowError::Network(_))
    }
}
