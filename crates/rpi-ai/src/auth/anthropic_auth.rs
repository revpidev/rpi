//! Port of the `anthropicApiKeyAuth` half of
//! `packages/ai/src/providers/anthropic.ts` @ pi 0.82.1 (2efa728), lines
//! 9-36 (the `anthropicProvider()` wiring belongs to the provider registry,
//! T13).
//!
//! Anthropic's api-key auth is non-standard: `ANTHROPIC_AUTH_TOKEN` must
//! reach requests as an `Authorization: Bearer` header rather than an api
//! key, so [`env_api_key_auth`](super::helpers::env_api_key_auth) cannot be
//! used.

use super::env_keys::{ANTHROPIC_API_KEY_ENV, ANTHROPIC_AUTH_TOKEN_ENV, ANTHROPIC_OAUTH_TOKEN_ENV};
use super::interaction::{AuthInteraction, AuthPrompt};
use super::resolve::ModelsError;
use super::types::{ApiKeyAuth, ApiKeyCredential, AuthContext, AuthResult, ModelAuth};
use crate::types::ProviderHeaders;

/// `anthropicApiKeyAuth()` — see module docs.
pub struct AnthropicApiKeyAuth;

/// `anthropicApiKeyAuth()`.
pub fn anthropic_api_key_auth() -> AnthropicApiKeyAuth {
    AnthropicApiKeyAuth
}

#[async_trait::async_trait]
impl ApiKeyAuth for AnthropicApiKeyAuth {
    fn name(&self) -> &str {
        "Anthropic API key"
    }

    fn supports_login(&self) -> bool {
        true
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<ApiKeyCredential, ModelsError> {
        let key = interaction
            .prompt(AuthPrompt::secret("Enter Anthropic API key"))
            .await?;
        Ok(ApiKeyCredential {
            key: Some(key),
            env: None,
        })
    }

    async fn resolve(
        &self,
        ctx: &dyn AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        // Stored key wins (`credential?.key` — empty strings are falsy).
        if let Some(credential) = credential {
            if let Some(key) = credential.key.clone().filter(|key| !key.is_empty()) {
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(key),
                        headers: None,
                        base_url: None,
                    },
                    env: credential.env.clone(),
                    source: Some("stored credential".to_owned()),
                }));
            }
        }

        // ANTHROPIC_AUTH_TOKEN → Authorization: Bearer header (not x-api-key).
        if let Some(auth_token) = ctx
            .env(ANTHROPIC_AUTH_TOKEN_ENV)
            .await
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: None,
                    headers: Some(ProviderHeaders::from([(
                        "Authorization".to_owned(),
                        Some(format!("Bearer {auth_token}")),
                    )])),
                    base_url: None,
                },
                env: None,
                source: Some(ANTHROPIC_AUTH_TOKEN_ENV.to_owned()),
            }));
        }

        for env_var in [ANTHROPIC_OAUTH_TOKEN_ENV, ANTHROPIC_API_KEY_ENV] {
            if let Some(api_key) = ctx.env(env_var).await.filter(|value| !value.is_empty()) {
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(api_key),
                        headers: None,
                        base_url: None,
                    },
                    env: None,
                    source: Some(env_var.to_owned()),
                }));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    //! Test intents ported from
    //! `packages/ai/test/anthropic-auth-token.test.ts`
    //! @ pi 0.82.1 (2efa728), same names in snake_case.
    //!
    //! Not ported here: the four `stream`-level cases ("uses Authorization
    //! headers without OAuth-mode request shaping", "threads authContext
    //! ANTHROPIC_AUTH_TOKEN through request headers", "preserves OAuth request
    //! shaping for ANTHROPIC_OAUTH_TOKEN", "lets explicit request headers
    //! override ANTHROPIC_AUTH_TOKEN") exercise the anthropic-messages
    //! adapter's request shaping against a mocked SDK, which is the
    //! adapter's territory (T03), not this auth module's.

    use std::collections::HashMap;

    use super::*;

    struct MapAuthContext(HashMap<String, String>);

    #[async_trait::async_trait]
    impl AuthContext for MapAuthContext {
        async fn env(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }

        async fn file_exists(&self, _path: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn resolves_anthropic_auth_token_as_a_bearer_authorization_header() {
        let auth = anthropic_api_key_auth();
        let ctx = MapAuthContext(HashMap::from([
            (ANTHROPIC_AUTH_TOKEN_ENV.to_owned(), "auth-token".to_owned()),
            (
                ANTHROPIC_OAUTH_TOKEN_ENV.to_owned(),
                "oauth-token".to_owned(),
            ),
            (ANTHROPIC_API_KEY_ENV.to_owned(), "api-key".to_owned()),
        ]));
        let result = auth
            .resolve(&ctx, None)
            .await
            .expect("resolve")
            .expect("configured");
        assert_eq!(result.auth.api_key, None);
        assert_eq!(
            result
                .auth
                .headers
                .as_ref()
                .and_then(|headers| headers.get("Authorization")),
            Some(&Some("Bearer auth-token".to_owned()))
        );
        assert_eq!(result.source.as_deref(), Some(ANTHROPIC_AUTH_TOKEN_ENV));
    }

    #[tokio::test]
    async fn preserves_anthropic_oauth_token_as_oauth_shaped_api_auth() {
        let auth = anthropic_api_key_auth();
        let ctx = MapAuthContext(HashMap::from([
            (
                ANTHROPIC_OAUTH_TOKEN_ENV.to_owned(),
                "oauth-token".to_owned(),
            ),
            (ANTHROPIC_API_KEY_ENV.to_owned(), "api-key".to_owned()),
        ]));
        let result = auth
            .resolve(&ctx, None)
            .await
            .expect("resolve")
            .expect("configured");
        assert_eq!(result.auth.api_key.as_deref(), Some("oauth-token"));
        assert_eq!(result.auth.headers, None);
        assert_eq!(result.source.as_deref(), Some(ANTHROPIC_OAUTH_TOKEN_ENV));
    }

    #[tokio::test]
    async fn stored_credential_wins_over_all_env_vars() {
        let auth = anthropic_api_key_auth();
        let ctx = MapAuthContext(HashMap::from([
            (ANTHROPIC_AUTH_TOKEN_ENV.to_owned(), "auth-token".to_owned()),
            (ANTHROPIC_API_KEY_ENV.to_owned(), "api-key".to_owned()),
        ]));
        let credential = ApiKeyCredential {
            key: Some("stored-key".to_owned()),
            env: None,
        };
        let result = auth
            .resolve(&ctx, Some(&credential))
            .await
            .expect("resolve")
            .expect("configured");
        assert_eq!(result.auth.api_key.as_deref(), Some("stored-key"));
        assert_eq!(result.auth.headers, None);
        assert_eq!(result.source.as_deref(), Some("stored credential"));

        // Nothing configured.
        let ctx = MapAuthContext(HashMap::new());
        assert!(auth.resolve(&ctx, None).await.expect("resolve").is_none());
    }
}
