//! Port of `packages/ai/src/auth/types.ts` @ pi 0.82.1 (2efa728) — T03
//! skeleton: credential types, store/auth interfaces. `AuthInteraction`,
//! login prompts and OAuth flows land with T04.
//!
//! Intentional differences: TS method interfaces become `async_trait` traits;
//! the `signal` becomes `CancellationToken`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::resolve::ModelsError;
use crate::types::{ProviderEnv, ProviderHeaders};

/// `ModelAuth` — request auth for a single model request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub base_url: Option<String>,
}

/// `ApiKeyCredential` — stored api-key credential. `env` holds provider-scoped
/// environment/config values such as Cloudflare account/gateway ids.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

/// `OAuthCredential` — stored canonical OAuth credential. Extension-provided
/// extra fields are preserved via the flattened `extra` map (`[key: string]:
/// unknown` upstream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    pub expires: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `Credential` — one type-tagged credential per provider (auth.json shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

impl Credential {
    pub fn credential_type(&self) -> CredentialType {
        match self {
            Credential::ApiKey(_) => CredentialType::ApiKey,
            Credential::OAuth(_) => CredentialType::Oauth,
        }
    }
}

/// `Credential["type"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialType {
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "oauth")]
    Oauth,
}

/// `CredentialInfo` — non-secret credential metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub credential_type: CredentialType,
}

/// Boxed future used by the store/auth traits.
pub type BoxFutureSend<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Callback for [`CredentialStore::modify`]: sees the current credential,
/// returns the new one (`None` leaves the entry unchanged).
pub type ModifyFn = Arc<
    dyn Fn(Option<Credential>) -> BoxFutureSend<'static, Result<Option<Credential>, ModelsError>>
        + Send
        + Sync,
>;

/// `CredentialStore` — app-owned credential storage keyed by provider id.
/// `modify` is the only write path (serialized read-modify-write).
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    /// Read the stored credential, possibly expired. `None` for missing.
    async fn read(&self, provider_id: &str) -> Result<Option<Credential>, ModelsError>;

    /// List stored credential metadata without resolving or exposing secrets.
    async fn list(&self) -> Result<Vec<CredentialInfo>, ModelsError>;

    /// Serialized write — the only write path. Resolves with the post-write
    /// credential (current when the callback returns `None`).
    async fn modify(
        &self,
        provider_id: &str,
        f: ModifyFn,
    ) -> Result<Option<Credential>, ModelsError>;

    /// Remove a credential (logout). Serialized against `modify`.
    async fn delete(&self, provider_id: &str) -> Result<(), ModelsError>;
}

/// `AuthContext` — environment access for auth resolution (injectable).
#[async_trait::async_trait]
pub trait AuthContext: Send + Sync {
    async fn env(&self, name: &str) -> Option<String>;
    /// Check whether a file exists. Supports a leading `~`.
    async fn file_exists(&self, path: &str) -> bool;
}

/// `defaultAuthContext` (auth/context.ts): process env + filesystem.
pub struct DefaultAuthContext;

#[async_trait::async_trait]
impl AuthContext for DefaultAuthContext {
    async fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    async fn file_exists(&self, path: &str) -> bool {
        let expanded = if let Some(rest) = path.strip_prefix("~/") {
            std::env::var("HOME")
                .map(|home| format!("{home}/{rest}"))
                .unwrap_or_else(|_| path.to_owned())
        } else {
            path.to_owned()
        };
        tokio::fs::try_exists(expanded).await.unwrap_or(false)
    }
}

/// `AuthResult` — result of resolving auth for a model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthResult {
    pub auth: ModelAuth,
    /// Provider-scoped environment/config values resolved from credentials
    /// and ambient context.
    pub env: Option<ProviderEnv>,
    /// Human-readable label for status UI: "ANTHROPIC_API_KEY", "OAuth",
    /// "~/.aws/credentials".
    pub source: Option<String>,
}

/// `AuthType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    ApiKey,
    Oauth,
}

/// `AuthCheck`.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthCheck {
    pub source: Option<String>,
    pub kind: AuthType,
}

/// `ApiKeyAuth` — api-key auth: stored key/provider env plus ambient sources.
///
/// T03 skeleton: only `resolve` (request-time auth). `login`/`check` arrive
/// with T04.
#[async_trait::async_trait]
pub trait ApiKeyAuth: Send + Sync {
    /// Display name, e.g. "Anthropic API key".
    fn name(&self) -> &str;

    /// Resolve auth from the stored credential and/or ambient sources, merging
    /// per field. `None` = not configured.
    async fn resolve(
        &self,
        ctx: &dyn AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthResult>, ModelsError>;
}

/// `OAuthAuth` — the `refresh`/`toAuth` split lets `Models` own the locked
/// refresh pattern. `login` arrives with T04.
#[async_trait::async_trait]
pub trait OAuthAuth: Send + Sync {
    /// Display name, e.g. "Anthropic (Claude Pro/Max)".
    fn name(&self) -> &str;

    /// Exchange the refresh token. Network call; errors on failure.
    /// `Models` runs this under the store lock.
    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: Option<&CancellationToken>,
    ) -> Result<OAuthCredential, ModelsError>;

    /// Side-effect-free derivation of request auth from a valid credential.
    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, ModelsError>;
}

/// `ProviderAuth`. At least one of `api_key`/`oauth` must be present.
#[derive(Clone, Default)]
pub struct ProviderAuth {
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}
