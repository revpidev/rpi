use std::sync::LazyLock;

use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};

use super::types::{AuthResult, Credential, CredentialType, ModelAuth, ProviderAuth};
use super::{ModelsError, ModelsErrorCode};

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CredentialGeneration(String);

impl CredentialGeneration {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CredentialGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialGeneration(..)")
    }
}

impl std::fmt::Display for CredentialGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Process-local request identity for provider and usage consumers. It is not
/// serializable: generation is salted per process and contains no credential
/// or account identifier in readable form.
#[derive(Clone)]
pub struct ResolvedRequestAuth {
    pub auth: ModelAuth,
    pub env: Option<crate::types::ProviderEnv>,
    pub source: Option<String>,
    pub credential_kind: CredentialType,
    pub effective_origin: String,
    pub generation: CredentialGeneration,
}

impl std::fmt::Debug for ResolvedRequestAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedRequestAuth")
            .field("auth", &self.auth)
            .field("env", &self.env.as_ref().map(|env| env.keys()))
            .field("source", &self.source)
            .field("credential_kind", &self.credential_kind)
            .field("effective_origin", &self.effective_origin)
            .field("generation", &self.generation)
            .finish()
    }
}

static PROCESS_SALT: LazyLock<Result<[u8; 32], ModelsError>> = LazyLock::new(|| {
    let mut salt = [0u8; 32];
    SystemRandom::new().fill(&mut salt).map_err(|_| {
        ModelsError::new(
            ModelsErrorCode::Auth,
            "Cannot initialize process-local credential generation: OS random source unavailable",
        )
    })?;
    Ok(salt)
});

fn origin(base_url: &str) -> Result<String, ModelsError> {
    let url = url::Url::parse(base_url).map_err(|error| {
        ModelsError::new(
            ModelsErrorCode::Auth,
            format!("Invalid request baseUrl: {error}"),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ModelsError::new(
            ModelsErrorCode::Auth,
            "Request baseUrl must be an HTTP(S) URL without userinfo",
        ));
    }
    Ok(url.origin().ascii_serialization())
}

pub(crate) fn validate_subscription_origin(
    provider_id: &str,
    auth: &ProviderAuth,
    official_base_url: Option<&str>,
    configured_base_url: &str,
) -> Result<(), ModelsError> {
    if !auth
        .oauth
        .as_ref()
        .is_some_and(|oauth| oauth.is_subscription())
    {
        return Ok(());
    }
    let configured = origin(configured_base_url)?;
    let official = official_base_url.map(origin).transpose()?.ok_or_else(|| {
        ModelsError::new(
            ModelsErrorCode::Auth,
            format!(
                "Provider {provider_id}: no official origin is registered for subscription OAuth"
            ),
        )
    })?;
    if configured != official {
        return Err(ModelsError::new(
            ModelsErrorCode::Auth,
            format!(
                "Provider {provider_id}: custom/proxy origin {configured} cannot inherit the official subscription identity ({official}); configure an API key explicitly or restore the official baseUrl"
            ),
        ));
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn generation(parts: &[&[u8]]) -> Result<CredentialGeneration, ModelsError> {
    let salt = PROCESS_SALT.as_ref().map_err(|error| error.clone())?;
    let mut hash = Sha256::new();
    hash.update(b"rpi-request-credential-v1\0");
    hash.update(salt);
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    Ok(CredentialGeneration(format!("{:x}", hash.finalize())))
}

fn account_id(credential: &super::OAuthCredential) -> Result<Option<String>, ModelsError> {
    let read = |name: &str| -> Result<Option<String>, ModelsError> {
        credential
            .extra
            .get(name)
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        ModelsError::new(
                            ModelsErrorCode::Auth,
                            format!("OAuth credential field {name} must be a non-empty string"),
                        )
                    })
            })
            .transpose()
    };
    let account_id = read("accountId")?;
    let snake_account_id = read("account_id")?;
    if account_id.is_some() && snake_account_id.is_some() && account_id != snake_account_id {
        return Err(ModelsError::new(
            ModelsErrorCode::Auth,
            "Conflicting accountId and account_id in OAuth credential",
        ));
    }
    let account_id = account_id.or(snake_account_id);
    let jwt_account_id = crate::auth::oauth::openai_codex::get_account_id(&credential.access);
    if account_id.is_some() && jwt_account_id.is_some() && account_id != jwt_account_id {
        return Err(ModelsError::new(
            ModelsErrorCode::Auth,
            "OAuth accountId does not match the access-token account",
        ));
    }
    Ok(account_id.or(jwt_account_id))
}

/// The validated account id of an OAuth credential (top-level `accountId` /
/// `account_id`, cross-checked with the Codex JWT). `openai-codex` requires
/// one; other providers may omit it.
pub fn oauth_account_id(
    credential: &super::OAuthCredential,
) -> Result<Option<String>, ModelsError> {
    account_id(credential)
}

fn ensure_oauth_snapshot_matches(auth: &ModelAuth, access: &str) -> Result<(), ModelsError> {
    if auth.api_key.as_deref().is_some_and(|key| key == access) {
        return Ok(());
    }
    let bearer = format!("Bearer {access}");
    if auth.headers.as_ref().is_some_and(|headers| {
        headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value.as_deref() == Some(&bearer)
        })
    }) {
        return Ok(());
    }
    Err(ModelsError::new(
        ModelsErrorCode::Auth,
        "Credential changed while resolving request auth; retry without reusing the previous identity",
    ))
}

impl ResolvedRequestAuth {
    /// Resolve the identity of an API key without reading the credential
    /// store or refreshing anything. Non-inference consumers (usage) use this
    /// to keep the cache key on the same key+origin the request would use.
    pub fn resolve_api_key_read_only(
        provider_id: &str,
        official_base_url: Option<&str>,
        configured_base_url: &str,
        key: &str,
    ) -> Result<Self, ModelsError> {
        let result = AuthResult {
            auth: ModelAuth {
                api_key: Some(key.to_owned()),
                ..Default::default()
            },
            env: None,
            source: Some("api key".to_owned()),
        };
        Self::resolve(
            provider_id,
            &ProviderAuth::default(),
            official_base_url,
            configured_base_url,
            result,
            None,
        )
    }

    /// Resolve the identity of a stored OAuth credential **without** any
    /// refresh: an expired credential is an error. Subscription usage must
    /// never rotate tokens or write auth state (ADR-0039).
    pub async fn resolve_oauth_read_only(
        provider_id: &str,
        provider_auth: &ProviderAuth,
        official_base_url: Option<&str>,
        configured_base_url: &str,
        credential: &super::OAuthCredential,
    ) -> Result<Self, ModelsError> {
        if credential.expires <= now_ms() {
            return Err(ModelsError::new(
                ModelsErrorCode::Oauth,
                format!("OAuth credential for {provider_id} is expired; sign in again"),
            ));
        }
        let oauth = provider_auth.oauth.as_ref().ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Auth,
                format!("Provider {provider_id} has no OAuth auth method"),
            )
        })?;
        let auth = oauth.to_auth(credential).await.map_err(|error| {
            ModelsError::with_cause(
                ModelsErrorCode::Oauth,
                format!("OAuth auth derivation failed for {provider_id}"),
                &error.message,
            )
        })?;
        let result = AuthResult {
            auth,
            env: None,
            source: Some("OAuth".to_owned()),
        };
        Self::resolve(
            provider_id,
            provider_auth,
            official_base_url,
            configured_base_url,
            result,
            Some(Credential::OAuth(credential.clone())),
        )
    }

    pub(crate) fn resolve(
        provider_id: &str,
        provider_auth: &ProviderAuth,
        official_base_url: Option<&str>,
        configured_base_url: &str,
        result: AuthResult,
        stored_credential: Option<Credential>,
    ) -> Result<Self, ModelsError> {
        let credential_kind = stored_credential
            .as_ref()
            .map(Credential::credential_type)
            .unwrap_or(CredentialType::ApiKey);
        let source_kind = result.source.as_deref().map(|source| {
            if source == "OAuth" {
                CredentialType::Oauth
            } else {
                CredentialType::ApiKey
            }
        });
        if source_kind.is_some_and(|kind| {
            kind != credential_kind
                || (kind == CredentialType::Oauth && stored_credential.is_none())
        }) {
            return Err(ModelsError::new(
                ModelsErrorCode::Auth,
                "Credential kind changed while resolving request auth; retry without reusing the previous identity",
            ));
        }
        if credential_kind == CredentialType::Oauth {
            validate_subscription_origin(
                provider_id,
                provider_auth,
                official_base_url,
                configured_base_url,
            )?;
        }
        let effective_origin = result
            .auth
            .base_url
            .as_deref()
            .filter(|url| !url.is_empty())
            .unwrap_or(configured_base_url);
        let effective_origin = origin(effective_origin)?;
        let generation = match stored_credential {
            Some(Credential::OAuth(credential)) => {
                ensure_oauth_snapshot_matches(&result.auth, &credential.access)?;
                let account_id = if provider_id == "openai-codex" {
                    Some(account_id(&credential)?.ok_or_else(|| {
                        ModelsError::new(
                            ModelsErrorCode::Auth,
                            "OpenAI Codex OAuth credential is missing accountId",
                        )
                    })?)
                } else {
                    None
                };
                generation(&[
                    b"oauth",
                    effective_origin.as_bytes(),
                    credential.access.as_bytes(),
                    &credential.expires.to_be_bytes(),
                    account_id.as_deref().unwrap_or_default().as_bytes(),
                ])
            }
            Some(Credential::ApiKey(credential)) => {
                let key = result
                    .auth
                    .api_key
                    .as_deref()
                    .or(credential.key.as_deref())
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| {
                        ModelsError::new(
                            ModelsErrorCode::Auth,
                            format!(
                                "Provider {provider_id}: resolved API key credential has no key"
                            ),
                        )
                    })?;
                generation(&[b"api_key", effective_origin.as_bytes(), key.as_bytes()])
            }
            None => {
                let key = result
                    .auth
                    .api_key
                    .as_deref()
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| {
                        ModelsError::new(
                            ModelsErrorCode::Auth,
                            format!("Provider {provider_id}: resolved auth has no API key"),
                        )
                    })?;
                generation(&[b"api_key", effective_origin.as_bytes(), key.as_bytes()])
            }
        }?;
        Ok(Self {
            auth: result.auth,
            env: result.env,
            source: result.source,
            credential_kind,
            effective_origin,
            generation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::OAuthCredential;
    use serde_json::Map;

    fn result(key: &str) -> AuthResult {
        AuthResult {
            auth: ModelAuth {
                api_key: Some(key.to_owned()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn generation_tracks_key_origin_and_oauth_rotation_without_exposing_secrets() {
        let auth = ProviderAuth::default();
        let first = ResolvedRequestAuth::resolve(
            "openrouter",
            &auth,
            None,
            "https://openrouter.ai/api/v1",
            result("first-secret"),
            None,
        )
        .expect("first");
        let second = ResolvedRequestAuth::resolve(
            "openrouter",
            &auth,
            None,
            "https://openrouter.ai/api/v1",
            result("second-secret"),
            None,
        )
        .expect("second");
        assert_ne!(first.generation, second.generation);
        assert_eq!(first.effective_origin, "https://openrouter.ai");
        let debug = format!("{first:?} {second:?}");
        assert!(!debug.contains("first-secret"));
        assert!(!debug.contains("second-secret"));

        let oauth = |access: &str, expires| {
            Credential::OAuth(OAuthCredential {
                refresh: "refresh-secret".to_owned(),
                access: access.to_owned(),
                expires,
                extra: Map::new(),
            })
        };
        let one = ResolvedRequestAuth::resolve(
            "openai",
            &auth,
            None,
            "https://api.openai.com/v1",
            result("access-one"),
            Some(oauth("access-one", 100)),
        )
        .expect("oauth one");
        let two = ResolvedRequestAuth::resolve(
            "openai",
            &auth,
            None,
            "https://api.openai.com/v1",
            result("access-one"),
            Some(oauth("access-one", 200)),
        )
        .expect("oauth two");
        assert_eq!(one.credential_kind, CredentialType::Oauth);
        assert_ne!(one.generation, two.generation);
    }

    #[test]
    fn codex_rejects_conflicting_accounts_and_subscription_rejects_proxy_origins() {
        let mut credential = OAuthCredential {
            refresh: "refresh-secret".to_owned(),
            access: "access-secret".to_owned(),
            expires: 100,
            extra: Map::new(),
        };
        credential
            .extra
            .insert("accountId".to_owned(), serde_json::json!("one"));
        credential
            .extra
            .insert("account_id".to_owned(), serde_json::json!("two"));
        let error = ResolvedRequestAuth::resolve(
            "openai-codex",
            &ProviderAuth::default(),
            None,
            "https://chatgpt.com/backend-api",
            result("access-secret"),
            Some(Credential::OAuth(credential)),
        )
        .expect_err("conflicting account");
        assert!(error.message.contains("Conflicting accountId"));

        let oauth = crate::auth::oauth::openai_codex::OpenAiCodexOAuth::new();
        let error = validate_subscription_origin(
            "openai-codex",
            &ProviderAuth {
                oauth: Some(std::sync::Arc::new(oauth)),
                ..Default::default()
            },
            Some("https://chatgpt.com/backend-api"),
            "https://proxy.example.com/v1",
        )
        .expect_err("proxy must fail");
        assert!(error.message.contains("cannot inherit"));
        assert!(!error.message.contains("access-secret"));
    }

    #[test]
    fn origin_and_credential_kind_conflicts_fail_closed_without_secret_echoes() {
        let error = ResolvedRequestAuth::resolve(
            "openrouter",
            &ProviderAuth::default(),
            None,
            "https://user:password@example.com/v1",
            result("key"),
            None,
        )
        .expect_err("userinfo must fail");
        assert!(!error.message.contains("https://"));
        assert!(!error.message.contains("example.com"));
        assert!(!error.message.contains("user:password"));

        let mut oauth_result = result("stored-secret");
        oauth_result.source = Some("OAuth".to_owned());
        let error = ResolvedRequestAuth::resolve(
            "openai-codex",
            &ProviderAuth::default(),
            None,
            "https://chatgpt.com/backend-api",
            oauth_result,
            Some(Credential::ApiKey(crate::auth::ApiKeyCredential {
                key: Some("stored-secret".to_owned()),
                env: None,
            })),
        )
        .expect_err("credential kind transition must fail");
        assert!(error.message.contains("Credential kind changed"));
        assert!(!error.message.contains("stored-secret"));
    }

    #[test]
    fn api_key_read_only_identity_uses_key_and_origin() {
        let one = ResolvedRequestAuth::resolve_api_key_read_only(
            "deepseek",
            Some("https://api.deepseek.com"),
            "https://api.deepseek.com/v1",
            "first-key",
        )
        .expect("first");
        let two = ResolvedRequestAuth::resolve_api_key_read_only(
            "deepseek",
            Some("https://api.deepseek.com"),
            "https://api.deepseek.com/v1",
            "second-key",
        )
        .expect("second");
        assert_eq!(one.credential_kind, CredentialType::ApiKey);
        assert_eq!(one.effective_origin, "https://api.deepseek.com");
        assert_ne!(one.generation, two.generation);
        let debug = format!("{one:?} {two:?}");
        assert!(!debug.contains("first-key"));
        assert!(!debug.contains("second-key"));
    }

    #[tokio::test]
    async fn oauth_read_only_rejects_expired_and_never_refreshes() {
        let oauth = crate::auth::oauth::anthropic::AnthropicOAuth::new();
        let provider_auth = ProviderAuth {
            api_key: None,
            oauth: Some(std::sync::Arc::new(oauth)),
        };
        let expired = OAuthCredential {
            refresh: "refresh-secret".to_owned(),
            access: "access-secret".to_owned(),
            expires: 0,
            extra: Map::new(),
        };
        let error = ResolvedRequestAuth::resolve_oauth_read_only(
            "anthropic",
            &provider_auth,
            Some("https://api.anthropic.com"),
            "https://api.anthropic.com",
            &expired,
        )
        .await
        .expect_err("expired");
        assert!(error.message.contains("expired"));
        assert!(!error.message.contains("access-secret"));
        assert!(!error.message.contains("refresh-secret"));

        let valid = OAuthCredential {
            refresh: "refresh-secret".to_owned(),
            access: "access-secret".to_owned(),
            expires: now_ms() + 3_600_000,
            extra: Map::new(),
        };
        let identity = ResolvedRequestAuth::resolve_oauth_read_only(
            "anthropic",
            &provider_auth,
            Some("https://api.anthropic.com"),
            "https://api.anthropic.com",
            &valid,
        )
        .await
        .expect("valid");
        assert_eq!(identity.credential_kind, CredentialType::Oauth);
        assert_eq!(identity.auth.api_key.as_deref(), Some("access-secret"));
        assert_eq!(identity.effective_origin, "https://api.anthropic.com");
        let debug = format!("{identity:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
    }

    #[test]
    fn oauth_account_id_reads_the_flattened_top_level_field() {
        let mut credential = OAuthCredential {
            refresh: "r".to_owned(),
            access: "a".to_owned(),
            expires: 1,
            extra: Map::new(),
        };
        assert_eq!(oauth_account_id(&credential).expect("none"), None);
        credential
            .extra
            .insert("accountId".to_owned(), serde_json::json!("acc-1"));
        assert_eq!(
            oauth_account_id(&credential).expect("account").as_deref(),
            Some("acc-1")
        );
        credential
            .extra
            .insert("account_id".to_owned(), serde_json::json!("acc-2"));
        assert!(oauth_account_id(&credential).is_err());
    }
}
