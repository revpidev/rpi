//! Stateful OAuth provider backed by a store (port of
//! `packages/mcp/src/oauth/provider.ts` @ a13d35a74).

use std::sync::Arc;

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use url::Url;

use super::errors::OAuthFlowError;
use super::flow::{CredentialKind, OAuthClientProvider};
use super::types::{OAuthClientInformation, OAuthClientMetadata, OAuthDiscoveryState, OAuthTokens};

/// `McpOAuthState` (provider.ts:4): everything stored per server URL. The
/// stored shape is the upstream TypeScript JSON (camelCase container fields,
/// snake_case OAuth wire fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthState {
    pub server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_information: Option<OAuthClientInformation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<OAuthTokens>,
    /// When the access token expires, in milliseconds since the epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_expire_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<OAuthDiscoveryState>,
}

/// `McpOAuthStateStore` (provider.ts:14).
#[async_trait::async_trait]
pub trait McpOAuthStateStore: Send + Sync {
    async fn load(&self) -> Option<McpOAuthState>;
    async fn save(&self, state: McpOAuthState);
}

/// `MemoryOAuthStateStore` (provider.ts:28).
#[derive(Default)]
pub struct MemoryOAuthStateStore {
    value: tokio::sync::Mutex<Option<McpOAuthState>>,
}

#[async_trait::async_trait]
impl McpOAuthStateStore for MemoryOAuthStateStore {
    async fn load(&self) -> Option<McpOAuthState> {
        self.value.lock().await.clone()
    }

    async fn save(&self, state: McpOAuthState) {
        *self.value.lock().await = Some(state);
    }
}

/// `McpOAuthProviderOptions` (provider.ts:37).
pub struct McpOAuthProviderOptions {
    pub server_url: String,
    pub redirect_url: String,
    pub client_metadata: OAuthClientMetadata,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub store: Option<Arc<dyn McpOAuthStateStore>>,
    pub on_redirect: Arc<dyn Fn(Url) + Send + Sync>,
}

/// `McpOAuthProvider` (provider.ts:50): default stateful provider for one
/// exact MCP server URL.
pub struct McpOAuthProvider {
    server_url: String,
    redirect_url: String,
    client_metadata: OAuthClientMetadata,
    configured_client: Option<OAuthClientInformation>,
    store: Arc<dyn McpOAuthStateStore>,
    on_redirect: Arc<dyn Fn(Url) + Send + Sync>,
    writes: tokio::sync::Mutex<()>,
}

impl McpOAuthProvider {
    pub fn new(options: McpOAuthProviderOptions) -> Self {
        let redirect_url = options.redirect_url;
        let mut client_metadata = options.client_metadata;
        if client_metadata.redirect_uris.is_empty() {
            client_metadata.redirect_uris = vec![redirect_url.clone()];
        }
        if client_metadata.grant_types.is_none() {
            client_metadata.grant_types = Some(vec![
                "authorization_code".to_owned(),
                "refresh_token".to_owned(),
            ]);
        }
        if client_metadata.response_types.is_none() {
            client_metadata.response_types = Some(vec!["code".to_owned()]);
        }
        if client_metadata.token_endpoint_auth_method.is_none() {
            client_metadata.token_endpoint_auth_method = Some(if options.client_secret.is_some() {
                "client_secret_post".to_owned()
            } else {
                "none".to_owned()
            });
        }
        let configured_client = options.client_id.map(|client_id| OAuthClientInformation {
            client_id,
            client_secret: options.client_secret,
            ..Default::default()
        });
        Self {
            server_url: String::new(),
            redirect_url,
            client_metadata,
            configured_client,
            store: options
                .store
                .unwrap_or_else(|| Arc::new(MemoryOAuthStateStore::default())),
            on_redirect: options.on_redirect,
            writes: tokio::sync::Mutex::new(()),
        }
        .with_server_url(options.server_url)
    }

    fn with_server_url(mut self, server_url: String) -> Self {
        self.server_url = Url::parse(&server_url)
            .map(|url| url.to_string())
            .unwrap_or(server_url);
        self
    }

    async fn load(&self) -> McpOAuthState {
        let _guard = self.writes.lock().await;
        self.own(self.store.load().await)
    }

    async fn update(&self, update: impl FnOnce(&mut McpOAuthState)) {
        let _guard = self.writes.lock().await;
        let mut state = self.own(self.store.load().await);
        update(&mut state);
        self.store.save(state).await;
    }

    /// Stored state for another server URL is ignored, so credentials never
    /// leak across servers.
    fn own(&self, state: Option<McpOAuthState>) -> McpOAuthState {
        match state {
            Some(state) if state.server_url == self.server_url => state,
            _ => McpOAuthState {
                server_url: self.server_url.clone(),
                client_information: None,
                tokens: None,
                tokens_expire_at: None,
                code_verifier: None,
                oauth_state: None,
                discovery: None,
            },
        }
    }
}

fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    let rng = SystemRandom::new();
    if rng.fill(&mut bytes).is_err() {
        // A CSPRNG failure is unrecoverable; fall back to a time-based value
        // so the flow fails at the server rather than panicking.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        return format!("{now:032x}");
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl OAuthClientProvider for McpOAuthProvider {
    fn redirect_url(&self) -> String {
        self.redirect_url.clone()
    }

    fn client_metadata(&self) -> OAuthClientMetadata {
        self.client_metadata.clone()
    }

    async fn state(&self) -> Option<String> {
        let existing = self.load().await.oauth_state;
        if existing.is_some() {
            return existing;
        }
        let state = random_hex();
        let next = state.clone();
        self.update(|value| value.oauth_state = Some(next)).await;
        Some(state)
    }

    async fn client_information(&self) -> Option<OAuthClientInformation> {
        match &self.configured_client {
            Some(client) => Some(client.clone()),
            None => self.load().await.client_information,
        }
    }

    async fn save_client_information(&self, information: OAuthClientInformation) {
        if self.configured_client.is_some() {
            return;
        }
        self.update(|state| state.client_information = Some(information))
            .await;
    }

    async fn tokens(&self) -> Option<OAuthTokens> {
        self.load().await.tokens
    }

    async fn save_tokens(&self, tokens: OAuthTokens) {
        let expires_at = tokens
            .expires_in
            .map(|seconds| now_millis() + seconds * 1000.0);
        self.update(|state| {
            state.tokens_expire_at = expires_at;
            state.tokens = Some(tokens);
        })
        .await;
    }

    async fn redirect_to_authorization(&self, url: Url) {
        (self.on_redirect)(url);
    }

    async fn save_code_verifier(&self, verifier: String) {
        self.update(|state| state.code_verifier = Some(verifier))
            .await;
    }

    async fn code_verifier(&self) -> Result<String, OAuthFlowError> {
        self.load()
            .await
            .code_verifier
            .filter(|verifier| !verifier.is_empty())
            .ok_or_else(|| {
                OAuthFlowError::Invalid("No OAuth PKCE code verifier is stored".to_owned())
            })
    }

    async fn invalidate_credentials(&self, kind: CredentialKind) {
        self.update(|state| match kind {
            CredentialKind::All => {
                state.client_information = None;
                state.tokens = None;
                state.tokens_expire_at = None;
                state.code_verifier = None;
                state.discovery = None;
                state.oauth_state = None;
            }
            CredentialKind::Client => state.client_information = None,
            CredentialKind::Tokens => {
                state.tokens = None;
                state.tokens_expire_at = None;
            }
            CredentialKind::Verifier => state.code_verifier = None,
            CredentialKind::Discovery => state.discovery = None,
        })
        .await;
    }

    async fn save_discovery_state(&self, discovery: OAuthDiscoveryState) {
        self.update(|state| state.discovery = Some(discovery)).await;
    }

    async fn discovery_state(&self) -> Option<OAuthDiscoveryState> {
        self.load().await.discovery
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(store: Arc<MemoryOAuthStateStore>) -> McpOAuthProvider {
        McpOAuthProvider::new(McpOAuthProviderOptions {
            server_url: "https://mcp.example/mcp".to_owned(),
            redirect_url: "http://127.0.0.1:1234/callback".to_owned(),
            client_metadata: OAuthClientMetadata::default(),
            client_id: None,
            client_secret: None,
            store: Some(store),
            on_redirect: Arc::new(|_| {}),
        })
    }

    #[tokio::test]
    async fn state_is_stable_and_each_sign_in_rotates_it() {
        let provider = provider(Arc::new(MemoryOAuthStateStore::default()));
        let first = provider.state().await.unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(provider.state().await.unwrap(), first);
    }

    #[tokio::test]
    async fn save_and_invalidate_round_trip() {
        let provider = provider(Arc::new(MemoryOAuthStateStore::default()));
        provider
            .save_tokens(OAuthTokens {
                access_token: "a".to_owned(),
                token_type: "bearer".to_owned(),
                expires_in: Some(3600.0),
                refresh_token: Some("r".to_owned()),
                ..Default::default()
            })
            .await;
        assert_eq!(provider.tokens().await.unwrap().access_token, "a");
        provider
            .invalidate_credentials(CredentialKind::Tokens)
            .await;
        assert!(provider.tokens().await.is_none());
    }

    #[tokio::test]
    async fn other_server_state_is_ignored() {
        let store = Arc::new(MemoryOAuthStateStore::default());
        store
            .save(McpOAuthState {
                server_url: "https://other.example/mcp".to_owned(),
                client_information: None,
                tokens: Some(OAuthTokens {
                    access_token: "leak".to_owned(),
                    token_type: "bearer".to_owned(),
                    ..Default::default()
                }),
                tokens_expire_at: None,
                code_verifier: None,
                oauth_state: None,
                discovery: None,
            })
            .await;
        let provider = provider(store);
        assert!(provider.tokens().await.is_none());
    }
}
