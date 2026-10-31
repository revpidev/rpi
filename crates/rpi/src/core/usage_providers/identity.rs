//! Credential identity for usage providers (V18-10 FR-C / 04 §4.3).
//!
//! First-party usage providers resolve their credential through the same
//! rpi auth state the model request uses, but **read-only**: no OAuth
//! refresh, no credential write, no third-party CLI directory. The resulting
//! [`UsageCredentials`] carries the process-local generation and effective
//! origin used in the cache key, plus the values that reach the provider
//! script through its environment only.
//!
//! Custom/legacy usage scripts have no host-proven identity and keep the old
//! provider-keyed cache behavior ([`LegacyCredentialResolver`]). A
//! first-party provider that cannot prove its identity fails closed with
//! `auth-unavailable` / `origin-mismatch` instead of showing another
//! account's cached quota.

use async_trait::async_trait;
use rpi_ai::auth::{Credential, ResolvedRequestAuth, oauth_account_id};
use rpi_ai::models::Provider;
use serde::{Deserialize, Serialize};

use super::UsageErrorKind;
use crate::core::agent_session::WeakAgentSession;

/// `RPI_USAGE_OAUTH_ACCESS` — the OAuth access token for one script run.
pub const USAGE_OAUTH_ACCESS_ENV: &str = "RPI_USAGE_OAUTH_ACCESS";
/// `RPI_USAGE_OAUTH_ACCOUNT_ID` — the validated OpenAI Codex account id.
pub const USAGE_OAUTH_ACCOUNT_ID_ENV: &str = "RPI_USAGE_OAUTH_ACCOUNT_ID";
/// Fallback API-key variable when a provider has no conventional env name.
pub const USAGE_API_KEY_ENV: &str = "RPI_USAGE_API_KEY";

/// Credential kind handed to the script (`credentialKind` in the stdin
/// context) and used by the registry to interpret the identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageCredentialKind {
    ApiKey,
    Oauth,
}

/// A host-proven credential identity for one first-party usage provider.
#[derive(Clone, PartialEq)]
pub struct UsageCredentials {
    pub credential_kind: UsageCredentialKind,
    /// Process-local salted hash; never a credential value.
    pub generation: String,
    pub effective_origin: String,
    /// Secret values for the child environment only.
    pub script_env: Vec<(String, String)>,
    /// Variable name announced to the script through `apiKeyEnv`.
    pub api_key_env: Option<String>,
}

impl std::fmt::Debug for UsageCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsageCredentials")
            .field("credential_kind", &self.credential_kind)
            .field("generation", &"[process-local]")
            .field("effective_origin", &self.effective_origin)
            .field(
                "script_env",
                &self
                    .script_env
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("api_key_env", &self.api_key_env)
            .finish()
    }
}

/// Resolve the credential identity of a usage provider. `Ok(None)` means the
/// provider is legacy/third-party and owns its credential handling; `Err` is
/// a classified failure that must stop the fetch before any script runs.
#[async_trait]
pub trait UsageCredentialResolver: Send + Sync {
    async fn resolve(
        &self,
        provider: &str,
        api_key_env: Option<&str>,
    ) -> Result<Option<UsageCredentials>, UsageErrorKind>;
}

/// Legacy resolver: no host-proven identity (third-party scripts).
#[derive(Debug, Default, Clone, Copy)]
pub struct LegacyCredentialResolver;

#[async_trait]
impl UsageCredentialResolver for LegacyCredentialResolver {
    async fn resolve(
        &self,
        _provider: &str,
        _api_key_env: Option<&str>,
    ) -> Result<Option<UsageCredentials>, UsageErrorKind> {
        Ok(None)
    }
}

/// Session-backed resolver for the eight first-party providers.
pub struct SessionUsageCredentialResolver {
    session: WeakAgentSession,
}

impl SessionUsageCredentialResolver {
    pub fn new(session: WeakAgentSession) -> Self {
        Self { session }
    }
}

#[async_trait]
impl UsageCredentialResolver for SessionUsageCredentialResolver {
    async fn resolve(
        &self,
        provider: &str,
        api_key_env: Option<&str>,
    ) -> Result<Option<UsageCredentials>, UsageErrorKind> {
        let Some(auth_spec) = builtin_usage_auth_spec(provider) else {
            return Ok(None);
        };
        let Some(session) = self.session.upgrade() else {
            return Err(UsageErrorKind::AuthUnavailable);
        };
        let runtime = session.model_runtime().clone();
        let current_model = session.model();

        match auth_spec.kind {
            UsageCredentialKind::ApiKey => {
                // Resolution order (03 §5.3): explicit usage apiKeyEnv, then
                // conventional environment variables, then auth.json's
                // api_key — all candidates before the next credential store.
                if let Some(name) = api_key_env
                    && let Some(key) = env_value(name)
                {
                    let candidate = auth_spec
                        .candidates
                        .iter()
                        .find(|candidate| candidate.env.contains(&name))
                        .unwrap_or(&auth_spec.candidates[0]);
                    return api_key_identity(
                        &runtime,
                        current_model.as_ref(),
                        candidate,
                        name,
                        &key,
                    )
                    .map(Some);
                }
                for candidate in auth_spec.candidates {
                    for name in candidate.env {
                        if let Some(key) = env_value(name) {
                            return api_key_identity(
                                &runtime,
                                current_model.as_ref(),
                                candidate,
                                name,
                                &key,
                            )
                            .map(Some);
                        }
                    }
                }
                for candidate in auth_spec.candidates {
                    let credential = match runtime
                        .credential_store()
                        .read(candidate.provider, None)
                        .await
                    {
                        Ok(credential) => credential,
                        Err(_) => return Err(UsageErrorKind::AuthUnavailable),
                    };
                    let Some(Credential::ApiKey(credential)) = credential else {
                        continue;
                    };
                    let Some(key) = credential.key.filter(|key| !key.is_empty()) else {
                        continue;
                    };
                    let name = candidate
                        .env
                        .first()
                        .map(|name| (*name).to_owned())
                        .or_else(|| api_key_env.map(str::to_owned))
                        .unwrap_or_else(|| USAGE_API_KEY_ENV.to_owned());
                    return api_key_identity(
                        &runtime,
                        current_model.as_ref(),
                        candidate,
                        &name,
                        &key,
                    )
                    .map(Some);
                }
                Err(UsageErrorKind::AuthUnavailable)
            }
            UsageCredentialKind::Oauth => {
                let mut last_error = UsageErrorKind::AuthUnavailable;
                for candidate in auth_spec.candidates {
                    let credential = match runtime
                        .credential_store()
                        .read(candidate.provider, None)
                        .await
                    {
                        Ok(Some(Credential::OAuth(credential))) => credential,
                        Ok(_) => continue,
                        Err(_) => return Err(UsageErrorKind::AuthUnavailable),
                    };
                    let Some(provider_config) = runtime.get_provider(candidate.provider) else {
                        continue;
                    };
                    let Some((configured_base_url, configured_origin, official_origin)) =
                        provider_origins(
                            provider_config.as_ref(),
                            current_model.as_ref(),
                            candidate.provider,
                        )
                    else {
                        last_error = UsageErrorKind::AuthUnavailable;
                        continue;
                    };
                    if configured_origin != official_origin {
                        last_error = UsageErrorKind::OriginMismatch;
                        continue;
                    }
                    let identity = match ResolvedRequestAuth::resolve_oauth_read_only(
                        candidate.provider,
                        provider_config.auth(),
                        provider_config.native_base_url(),
                        &configured_base_url,
                        &credential,
                    )
                    .await
                    {
                        Ok(identity) => identity,
                        Err(_) => {
                            last_error = UsageErrorKind::AuthUnavailable;
                            continue;
                        }
                    };
                    let mut script_env =
                        vec![(USAGE_OAUTH_ACCESS_ENV.to_owned(), credential.access.clone())];
                    if candidate.provider == "openai-codex" {
                        match oauth_account_id(&credential) {
                            Ok(Some(account_id)) => {
                                script_env.push((USAGE_OAUTH_ACCOUNT_ID_ENV.to_owned(), account_id))
                            }
                            Ok(None) | Err(_) => {
                                last_error = UsageErrorKind::AuthUnavailable;
                                continue;
                            }
                        }
                    }
                    return Ok(Some(UsageCredentials {
                        credential_kind: UsageCredentialKind::Oauth,
                        generation: identity.generation.as_str().to_owned(),
                        effective_origin: identity.effective_origin,
                        script_env,
                        api_key_env: None,
                    }));
                }
                Err(last_error)
            }
        }
    }
}

fn api_key_identity(
    runtime: &crate::core::model_runtime::ModelRuntime,
    current_model: Option<&rpi_ai::types::Model>,
    candidate: &UsageAuthCandidate,
    env_name: &str,
    key: &str,
) -> Result<UsageCredentials, UsageErrorKind> {
    let Some(provider_config) = runtime.get_provider(candidate.provider) else {
        return Err(UsageErrorKind::AuthUnavailable);
    };
    let Some((configured_base_url, configured_origin, official_origin)) =
        provider_origins(provider_config.as_ref(), current_model, candidate.provider)
    else {
        return Err(UsageErrorKind::AuthUnavailable);
    };
    if configured_origin != official_origin {
        return Err(UsageErrorKind::OriginMismatch);
    }
    let identity = ResolvedRequestAuth::resolve_api_key_read_only(
        candidate.provider,
        provider_config.native_base_url(),
        &configured_base_url,
        key,
    )
    .map_err(|_| UsageErrorKind::AuthUnavailable)?;
    Ok(UsageCredentials {
        credential_kind: UsageCredentialKind::ApiKey,
        generation: identity.generation.as_str().to_owned(),
        effective_origin: identity.effective_origin,
        script_env: vec![(env_name.to_owned(), key.to_owned())],
        api_key_env: Some(env_name.to_owned()),
    })
}

fn provider_origins(
    provider: &dyn Provider,
    current_model: Option<&rpi_ai::types::Model>,
    provider_id: &str,
) -> Option<(String, String, String)> {
    let configured = configured_base_url(
        provider,
        current_model.filter(|model| model.provider == provider_id),
    )?;
    let official = official_base_url(provider)?;
    let configured_origin = origin_of(&configured)?;
    let official_origin = origin_of(&official)?;
    Some((configured, configured_origin, official_origin))
}

/// One credential source for a first-party usage provider: the rpi model
/// provider id whose auth state is shared, plus its conventional API-key
/// environment variable names.
struct UsageAuthCandidate {
    provider: &'static str,
    env: &'static [&'static str],
}

struct UsageAuthSpec {
    kind: UsageCredentialKind,
    candidates: &'static [UsageAuthCandidate],
}

const DEEPSEEK: UsageAuthSpec = UsageAuthSpec {
    kind: UsageCredentialKind::ApiKey,
    candidates: &[UsageAuthCandidate {
        provider: "deepseek",
        env: &["DEEPSEEK_API_KEY"],
    }],
};

const GLM_CODING_PLAN: UsageAuthSpec = UsageAuthSpec {
    kind: UsageCredentialKind::ApiKey,
    candidates: &[
        UsageAuthCandidate {
            provider: "zai",
            env: &["ZAI_API_KEY", "GLM_API_KEY"],
        },
        UsageAuthCandidate {
            provider: "zai-coding-cn",
            env: &["ZAI_CODING_CN_API_KEY"],
        },
    ],
};

const MINIMAX_TOKEN_PLAN: UsageAuthSpec = UsageAuthSpec {
    kind: UsageCredentialKind::ApiKey,
    candidates: &[
        UsageAuthCandidate {
            provider: "minimax",
            env: &["MINIMAX_API_KEY"],
        },
        UsageAuthCandidate {
            provider: "minimax-cn",
            env: &["MINIMAX_CN_API_KEY"],
        },
    ],
};

const KIMI_CODE: UsageAuthSpec = UsageAuthSpec {
    kind: UsageCredentialKind::ApiKey,
    candidates: &[UsageAuthCandidate {
        provider: "kimi-coding",
        env: &["KIMI_API_KEY", "KIMI_CODING_API_KEY"],
    }],
};

const OPENCODE_GO: UsageAuthSpec = UsageAuthSpec {
    kind: UsageCredentialKind::ApiKey,
    candidates: &[UsageAuthCandidate {
        provider: "opencode-go",
        env: &["OPENCODE_API_KEY"],
    }],
};

const ANTHROPIC: UsageAuthSpec = UsageAuthSpec {
    kind: UsageCredentialKind::Oauth,
    candidates: &[UsageAuthCandidate {
        provider: "anthropic",
        env: &[],
    }],
};

const OPENAI_CODEX: UsageAuthSpec = UsageAuthSpec {
    kind: UsageCredentialKind::Oauth,
    candidates: &[UsageAuthCandidate {
        provider: "openai-codex",
        env: &[],
    }],
};

const XAI: UsageAuthSpec = UsageAuthSpec {
    kind: UsageCredentialKind::Oauth,
    candidates: &[UsageAuthCandidate {
        provider: "xai",
        env: &[],
    }],
};

fn builtin_usage_auth_spec(provider: &str) -> Option<&'static UsageAuthSpec> {
    Some(match provider {
        "deepseek" => &DEEPSEEK,
        "glm-coding-plan" => &GLM_CODING_PLAN,
        "minimax-token-plan" => &MINIMAX_TOKEN_PLAN,
        "kimi-code" => &KIMI_CODE,
        "opencode-go" => &OPENCODE_GO,
        "anthropic" => &ANTHROPIC,
        "openai-codex" => &OPENAI_CODEX,
        "xai" => &XAI,
        _ => return None,
    })
}

/// Whether `provider` is one of the eight first-party quota providers.
pub fn is_builtin_usage_provider(provider: &str) -> bool {
    builtin_usage_auth_spec(provider).is_some()
}

/// The credential kind the first-party provider requires.
pub fn builtin_usage_credential_kind(provider: &str) -> Option<UsageCredentialKind> {
    builtin_usage_auth_spec(provider).map(|spec| spec.kind)
}

/// The rpi model-provider ids whose auth state a usage provider shares.
pub fn builtin_usage_auth_providers(provider: &str) -> Option<Vec<&'static str>> {
    builtin_usage_auth_spec(provider).map(|spec| {
        spec.candidates
            .iter()
            .map(|candidate| candidate.provider)
            .collect()
    })
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn configured_base_url(
    provider: &dyn Provider,
    current_model: Option<&rpi_ai::types::Model>,
) -> Option<String> {
    current_model
        .map(|model| model.base_url.clone())
        .or_else(|| provider.base_url().map(str::to_owned))
        .or_else(|| {
            provider
                .get_models()
                .first()
                .map(|model| model.base_url.clone())
        })
}

fn official_base_url(provider: &dyn Provider) -> Option<String> {
    provider.native_base_url().map(str::to_owned).or_else(|| {
        provider
            .get_models()
            .first()
            .map(|model| model.base_url.clone())
    })
}

fn origin_of(base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_matches_the_eight_provider_decision() {
        let providers = [
            "deepseek",
            "glm-coding-plan",
            "minimax-token-plan",
            "kimi-code",
            "opencode-go",
            "anthropic",
            "openai-codex",
            "xai",
        ];
        for provider in providers {
            assert!(is_builtin_usage_provider(provider), "{provider}");
            assert!(builtin_usage_credential_kind(provider).is_some());
            assert!(!builtin_usage_auth_providers(provider).unwrap().is_empty());
        }
        assert!(!is_builtin_usage_provider("opencode"));
        assert!(!is_builtin_usage_provider("claude"));
        assert!(!is_builtin_usage_provider("custom"));
        assert_eq!(
            builtin_usage_credential_kind("anthropic"),
            Some(UsageCredentialKind::Oauth)
        );
        assert_eq!(
            builtin_usage_credential_kind("deepseek"),
            Some(UsageCredentialKind::ApiKey)
        );
        assert_eq!(
            builtin_usage_auth_providers("glm-coding-plan"),
            Some(vec!["zai", "zai-coding-cn"])
        );
    }

    #[test]
    fn origin_normalization_rejects_userinfo_and_non_http() {
        assert_eq!(
            origin_of("https://api.deepseek.com/v1?x=1").as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(
            origin_of("http://localhost:1234/v1").as_deref(),
            Some("http://localhost:1234")
        );
        assert!(origin_of("https://user:pass@example.com/v1").is_none());
        assert!(origin_of("file:///tmp/x").is_none());
        assert!(origin_of("not a url").is_none());
    }

    #[test]
    fn credentials_debug_never_prints_secret_values() {
        let credentials = UsageCredentials {
            credential_kind: UsageCredentialKind::Oauth,
            generation: "gen".to_owned(),
            effective_origin: "https://api.anthropic.com".to_owned(),
            script_env: vec![(USAGE_OAUTH_ACCESS_ENV.to_owned(), "secret".to_owned())],
            api_key_env: None,
        };
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains(USAGE_OAUTH_ACCESS_ENV));
        assert!(debug.contains("[process-local]"));
    }

    #[tokio::test]
    async fn legacy_resolver_returns_none() {
        let registry = LegacyCredentialResolver;
        assert_eq!(registry.resolve("custom", None).await, Ok(None));
    }
}
