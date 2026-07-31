//! Port of `packages/ai/src/auth/resolve.ts` @ pi 0.82.1 (2efa728).
//!
//! Shared auth resolution: a stored credential owns the provider (hit stops),
//! ambient/env is consulted only when nothing is stored. No silent env
//! fallback after a failed refresh. OAuth refresh runs under the store lock
//! with double-checked expiry.

use std::sync::Arc;

use super::types::{
    ApiKeyCredential, AuthContext, AuthResult, Credential, CredentialStore, OAuthCredential,
    ProviderAuth,
};
use crate::types::ProviderEnv;

/// `ModelsErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelsErrorCode {
    ModelSource,
    ModelValidation,
    Provider,
    Stream,
    Auth,
    Oauth,
}

impl ModelsErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelsErrorCode::ModelSource => "model_source",
            ModelsErrorCode::ModelValidation => "model_validation",
            ModelsErrorCode::Provider => "provider",
            ModelsErrorCode::Stream => "stream",
            ModelsErrorCode::Auth => "auth",
            ModelsErrorCode::Oauth => "oauth",
        }
    }
}

/// `ModelsError`.
#[derive(Debug, Clone)]
pub struct ModelsError {
    pub code: ModelsErrorCode,
    pub message: String,
}

impl ModelsError {
    pub fn new(code: ModelsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `withCauseDetail`: callers surface `message` only, so keep the
    /// underlying reason in it.
    pub fn with_cause(code: ModelsErrorCode, message: impl Into<String>, cause: &str) -> Self {
        let message = message.into();
        let detail = cause.trim();
        let message = if detail.is_empty() || message.contains(detail) {
            message
        } else {
            format!("{message}: {detail}")
        };
        Self { code, message }
    }
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ModelsError {}

/// `AuthResolutionOverrides`.
#[derive(Debug, Clone, Default)]
pub struct AuthResolutionOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
}

/// `overlayEnvAuthContext`: request-scoped env overrides win over the base.
struct OverlayEnvAuthContext<'a> {
    base: &'a dyn AuthContext,
    env: &'a ProviderEnv,
}

#[async_trait::async_trait]
impl AuthContext for OverlayEnvAuthContext<'_> {
    async fn env(&self, name: &str) -> Option<String> {
        match self.env.get(name) {
            Some(value) => Some(value.clone()),
            None => self.base.env(name).await,
        }
    }

    async fn file_exists(&self, path: &str) -> bool {
        self.base.file_exists(path).await
    }
}

/// `resolveProviderAuth` — see module docs.
pub async fn resolve_provider_auth(
    provider_id: &str,
    auth: &ProviderAuth,
    credentials: &Arc<dyn CredentialStore>,
    auth_context: &Arc<dyn AuthContext>,
    overrides: Option<&AuthResolutionOverrides>,
) -> Result<Option<AuthResult>, ModelsError> {
    let overlay;
    let request_auth_context: &dyn AuthContext = match overrides.and_then(|o| o.env.as_ref()) {
        Some(env) => {
            overlay = OverlayEnvAuthContext {
                base: auth_context.as_ref(),
                env,
            };
            &overlay
        }
        None => auth_context.as_ref(),
    };

    if let Some(api_key) = overrides.and_then(|o| o.api_key.clone()) {
        if let Some(api_key_auth) = &auth.api_key {
            let credential = ApiKeyCredential {
                key: Some(api_key),
                env: overrides.and_then(|o| o.env.clone()),
            };
            return resolve_api_key(
                request_auth_context,
                api_key_auth.as_ref(),
                provider_id,
                Some(&credential),
            )
            .await;
        }
    }

    let stored = read_credential(credentials, provider_id).await?;
    if let Some(stored) = stored {
        match (&stored, &auth.oauth, &auth.api_key) {
            (Credential::OAuth(oauth_credential), Some(oauth), _) => {
                return resolve_stored_oauth(
                    credentials,
                    provider_id,
                    oauth.clone(),
                    oauth_credential,
                )
                .await;
            }
            (Credential::ApiKey(api_key_credential), _, Some(api_key_auth)) => {
                let credential = match overrides.and_then(|o| o.env.as_ref()) {
                    Some(env) => {
                        let mut merged = api_key_credential.clone();
                        let mut base = merged.env.unwrap_or_default();
                        base.extend(env.clone());
                        merged.env = Some(base);
                        merged
                    }
                    None => api_key_credential.clone(),
                };
                return resolve_api_key(
                    request_auth_context,
                    api_key_auth.as_ref(),
                    provider_id,
                    Some(&credential),
                )
                .await;
            }
            _ => return Ok(None),
        }
    }

    // Ambient (env vars, AWS profiles, ADC files).
    match &auth.api_key {
        Some(api_key_auth) => {
            resolve_api_key(
                request_auth_context,
                api_key_auth.as_ref(),
                provider_id,
                None,
            )
            .await
        }
        None => Ok(None),
    }
}

/// `resolveStoredOAuth`: double-checked locking — valid tokens cost zero
/// locks; expired tokens lock, re-check under the lock, refresh once, persist
/// before release.
async fn resolve_stored_oauth(
    credentials: &Arc<dyn CredentialStore>,
    provider_id: &str,
    oauth: Arc<dyn super::types::OAuthAuth>,
    stored: &OAuthCredential,
) -> Result<Option<AuthResult>, ModelsError> {
    let mut credential = stored.clone();

    if now_ms() >= credential.expires {
        let provider_id_owned = provider_id.to_owned();
        let oauth_for_refresh = oauth.clone();
        let post = credentials
            .modify(
                provider_id,
                Arc::new(move |current| {
                    let provider_id = provider_id_owned.clone();
                    let oauth = oauth_for_refresh.clone();
                    Box::pin(async move {
                        let Some(Credential::OAuth(current)) = current else {
                            return Ok(None); // logged out meanwhile
                        };
                        if now_ms() < current.expires {
                            return Ok(None); // another process/request refreshed
                        }
                        oauth
                            .refresh(&current, None)
                            .await
                            .map_err(|error| {
                                ModelsError::with_cause(
                                    ModelsErrorCode::Oauth,
                                    format!("OAuth refresh failed for {provider_id}"),
                                    &error.message,
                                )
                            })
                            .map(|c| Some(Credential::OAuth(c)))
                    })
                }),
            )
            .await
            .map_err(|error| match error.code {
                ModelsErrorCode::Oauth => error,
                _ => ModelsError::with_cause(
                    ModelsErrorCode::Auth,
                    format!("Credential store modify failed for {provider_id}"),
                    &error.message,
                ),
            })?;
        let Some(Credential::OAuth(post)) = post else {
            return Ok(None); // logged out meanwhile
        };
        credential = post;
    }

    oauth
        .to_auth(&credential)
        .await
        .map(|auth| {
            Some(AuthResult {
                auth,
                env: None,
                source: Some("OAuth".to_owned()),
            })
        })
        .map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::Oauth,
                format!("OAuth auth derivation failed for {provider_id}"),
                &error.message,
            )
        })
}

async fn resolve_api_key(
    _auth_context: &dyn AuthContext,
    api_key: &dyn super::types::ApiKeyAuth,
    provider_id: &str,
    credential: Option<&ApiKeyCredential>,
) -> Result<Option<AuthResult>, ModelsError> {
    api_key
        .resolve(_auth_context, credential)
        .await
        .map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::Auth,
                format!("API key auth failed for provider {provider_id}"),
                &error.message,
            )
        })
}

async fn read_credential(
    credentials: &Arc<dyn CredentialStore>,
    provider_id: &str,
) -> Result<Option<Credential>, ModelsError> {
    credentials.read(provider_id).await.map_err(|error| {
        ModelsError::with_cause(
            ModelsErrorCode::Auth,
            format!("Credential store read failed for {provider_id}"),
            &error.message,
        )
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
