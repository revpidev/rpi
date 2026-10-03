//! Auth provider seam for the streamable HTTP transport (port of
//! `packages/mcp/src/auth-provider.ts` @ a13d35a74).

use async_trait::async_trait;
use url::Url;

use crate::protocol::McpError;

/// `UnauthorizedContext` (auth-provider.ts:3): the 401 (or 403 asking for
/// more scope) response and the token it carried. A different current token
/// means another request already refreshed it.
#[derive(Debug, Clone)]
pub struct UnauthorizedContext {
    pub status: u16,
    pub www_authenticate: Option<String>,
    pub server_url: Url,
    /// Access token the rejected request carried, if any.
    pub token: Option<String>,
}

/// `AuthProvider` (auth-provider.ts:11): supplies bearer tokens to the HTTP
/// transport and may refresh them after a 401.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    async fn token(&self) -> Option<String>;

    /// Called once for an unauthorized response; the request is retried with
    /// whatever credentials the call left behind. An error aborts the
    /// request (upstream `McpOAuthAuthorizationRequiredError`).
    async fn on_unauthorized(&self, _context: UnauthorizedContext) -> Result<(), McpError> {
        Ok(())
    }
}
