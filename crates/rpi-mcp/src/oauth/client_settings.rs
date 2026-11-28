use std::sync::Arc;

use url::Url;

use super::provider::{
    ClientMetadataDocumentFn, McpOAuthProvider, McpOAuthProviderOptions, McpOAuthStateStore,
};
use super::types::{OAuthClientInformation, OAuthClientMetadata};

#[derive(Debug, Clone, Default)]
pub struct McpOAuthSettings {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub callback_port: Option<u32>,
    pub callback_url: Option<String>,
    pub scope: Option<String>,
    pub client_name: Option<String>,
    pub client_registration: Option<String>,
    pub auth_server_metadata_url: Option<Url>,
}

#[derive(Clone)]
pub struct McpOAuthClientOptions {
    pub server_url: String,
    pub settings: McpOAuthSettings,
    pub client_metadata: OAuthClientMetadata,
    pub client_metadata_document: Option<ClientMetadataDocumentFn>,
    pub store: Arc<dyn McpOAuthStateStore>,
    pub client: Option<reqwest::Client>,
}

impl McpOAuthClientOptions {
    pub fn new(server_url: impl Into<String>, store: Arc<dyn McpOAuthStateStore>) -> Self {
        Self {
            server_url: server_url.into(),
            settings: McpOAuthSettings::default(),
            client_metadata: OAuthClientMetadata::default(),
            client_metadata_document: None,
            store,
            client: None,
        }
    }
}

pub fn create_oauth_client_provider(
    options: &McpOAuthClientOptions,
    redirect_url: String,
    on_redirect: Arc<dyn Fn(Url) + Send + Sync>,
) -> McpOAuthProvider {
    let mut client_metadata = options.client_metadata.clone();
    if client_metadata.scope.is_none() {
        client_metadata.scope = options.settings.scope.clone();
    }
    if client_metadata.client_name.is_none() {
        client_metadata.client_name = options.settings.client_name.clone();
    }
    McpOAuthProvider::new(McpOAuthProviderOptions {
        server_url: options.server_url.clone(),
        redirect_url,
        client_metadata,
        client_metadata_document: options.client_metadata_document.clone(),
        client_id: options.settings.client_id.clone(),
        client_secret: options.settings.client_secret.clone(),
        store: Some(options.store.clone()),
        on_redirect,
    })
}

pub fn registered_redirect_urls(client: Option<&OAuthClientInformation>) -> Vec<String> {
    client
        .map(|client| client.redirect_uris.clone())
        .unwrap_or_default()
}
