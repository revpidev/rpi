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

#[cfg(test)]
mod tests {
    //! Self-check list (docs/plan/v0.1/T04-rpi-ai-auth.md) resolution-chain assertions;
    //! expected semantics compared item by item against
    //! `packages/ai/src/auth/resolve.ts` @ pi 0.82.1 (2efa728):
    //! an explicit apiKey returns before `readCredential`; a stored credential owns its
    //! provider (no matching handler → undefined, never falls back to ambient); OAuth
    //! expiry is double-checked inside the modify lock, and a failed refresh throws
    //! `ModelsError("oauth")` without falling back
    //! env。

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use serde_json::Map;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::auth::credential_store::InMemoryCredentialStore;
    use crate::auth::types::{
        ApiKeyAuth, CredentialInfo, CredentialStore, ModelAuth, ModifyFn, OAuthAuth,
    };

    const ENV_VAR: &str = "TEST_RESOLVE_API_KEY";

    // ------------------------------------------------------------------
    // Fakes
    // ------------------------------------------------------------------

    /// Records `env()` calls; `env_values` stands in for the process env.
    struct RecordingAuthContext {
        env_values: HashMap<String, String>,
        env_calls: Mutex<Vec<String>>,
    }

    impl RecordingAuthContext {
        fn with(entries: &[(&str, &str)]) -> Self {
            Self {
                env_values: entries
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                env_calls: Mutex::new(Vec::new()),
            }
        }

        fn empty() -> Self {
            Self::with(&[])
        }

        fn env_calls(&self) -> Vec<String> {
            self.env_calls.lock().expect("env calls").clone()
        }
    }

    #[async_trait::async_trait]
    impl AuthContext for RecordingAuthContext {
        async fn env(&self, name: &str) -> Option<String> {
            self.env_calls
                .lock()
                .expect("env calls")
                .push(name.to_owned());
            self.env_values.get(name).cloned()
        }

        async fn file_exists(&self, _path: &str) -> bool {
            false
        }
    }

    /// Mirrors upstream `envApiKeyAuth.resolve` (auth/helpers.ts): a stored
    /// key wins, otherwise the configured env var resolves through `ctx.env`.
    /// Records whether it was invoked with a stored credential.
    struct FakeApiKeyAuth {
        received_credential: Mutex<Vec<bool>>,
    }

    impl FakeApiKeyAuth {
        fn new() -> Self {
            Self {
                received_credential: Mutex::new(Vec::new()),
            }
        }

        fn received_credential(&self) -> Vec<bool> {
            self.received_credential.lock().expect("received").clone()
        }
    }

    #[async_trait::async_trait]
    impl ApiKeyAuth for FakeApiKeyAuth {
        fn name(&self) -> &str {
            "Test API key"
        }

        async fn resolve(
            &self,
            ctx: &dyn AuthContext,
            credential: Option<&ApiKeyCredential>,
        ) -> Result<Option<AuthResult>, ModelsError> {
            self.received_credential
                .lock()
                .expect("received")
                .push(credential.is_some());
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
            if let Some(value) = ctx.env(ENV_VAR).await.filter(|value| !value.is_empty()) {
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(value),
                        headers: None,
                        base_url: None,
                    },
                    env: None,
                    source: Some(ENV_VAR.to_owned()),
                }));
            }
            Ok(None)
        }
    }

    /// Counts `refresh` calls; `to_auth` derives `apiKey = access` like the
    /// upstream anthropic OAuth handler.
    struct FakeOAuthAuth {
        refresh_calls: AtomicUsize,
        refresh_error: Option<String>,
    }

    impl FakeOAuthAuth {
        fn succeeding() -> Self {
            Self {
                refresh_calls: AtomicUsize::new(0),
                refresh_error: None,
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                refresh_calls: AtomicUsize::new(0),
                refresh_error: Some(message.to_owned()),
            }
        }

        fn refresh_calls(&self) -> usize {
            self.refresh_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl OAuthAuth for FakeOAuthAuth {
        fn name(&self) -> &str {
            "Test OAuth"
        }

        async fn refresh(
            &self,
            credential: &OAuthCredential,
            _signal: Option<&CancellationToken>,
        ) -> Result<OAuthCredential, ModelsError> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            match &self.refresh_error {
                Some(message) => Err(ModelsError::new(ModelsErrorCode::Oauth, message.clone())),
                None => Ok(OAuthCredential {
                    refresh: credential.refresh.clone(),
                    access: "refreshed-access".to_owned(),
                    expires: now_ms() + 60_000,
                    extra: Map::new(),
                }),
            }
        }

        async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, ModelsError> {
            Ok(ModelAuth {
                api_key: Some(credential.access.clone()),
                headers: None,
                base_url: None,
            })
        }
    }

    /// `CredentialStore` wrapper counting `read` calls (the explicit-apiKey
    /// path must return before any store read, resolve.ts:54-60).
    struct CountingStore {
        inner: InMemoryCredentialStore,
        reads: AtomicUsize,
    }

    impl CountingStore {
        fn new(inner: InMemoryCredentialStore) -> Self {
            Self {
                inner,
                reads: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl CredentialStore for CountingStore {
        async fn read(&self, provider_id: &str) -> Result<Option<Credential>, ModelsError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.read(provider_id).await
        }

        async fn list(&self) -> Result<Vec<CredentialInfo>, ModelsError> {
            self.inner.list().await
        }

        async fn modify(
            &self,
            provider_id: &str,
            f: ModifyFn,
        ) -> Result<Option<Credential>, ModelsError> {
            self.inner.modify(provider_id, f).await
        }

        async fn delete(&self, provider_id: &str) -> Result<(), ModelsError> {
            self.inner.delete(provider_id).await
        }
    }

    /// Simulates "another process refreshed meanwhile": `modify` swaps in a
    /// fresh credential *before* invoking the callback, so the callback's
    /// authoritative re-check sees an unexpired token (resolve.ts:107).
    struct SwapOnModifyStore {
        inner: InMemoryCredentialStore,
        swap: Mutex<Option<Credential>>,
    }

    impl SwapOnModifyStore {
        fn new(inner: InMemoryCredentialStore, swap: Credential) -> Self {
            Self {
                inner,
                swap: Mutex::new(Some(swap)),
            }
        }
    }

    #[async_trait::async_trait]
    impl CredentialStore for SwapOnModifyStore {
        async fn read(&self, provider_id: &str) -> Result<Option<Credential>, ModelsError> {
            self.inner.read(provider_id).await
        }

        async fn list(&self) -> Result<Vec<CredentialInfo>, ModelsError> {
            self.inner.list().await
        }

        async fn modify(
            &self,
            provider_id: &str,
            f: ModifyFn,
        ) -> Result<Option<Credential>, ModelsError> {
            let swap = self.swap.lock().expect("swap").take();
            if let Some(credential) = swap {
                let swapped = credential.clone();
                self.inner
                    .modify(
                        provider_id,
                        Arc::new(move |_| {
                            let swapped = swapped.clone();
                            Box::pin(async move { Ok(Some(swapped)) })
                        }),
                    )
                    .await?;
            }
            self.inner.modify(provider_id, f).await
        }

        async fn delete(&self, provider_id: &str) -> Result<(), ModelsError> {
            self.inner.delete(provider_id).await
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn api_key_credential(key: Option<&str>) -> Credential {
        Credential::ApiKey(ApiKeyCredential {
            key: key.map(str::to_owned),
            env: None,
        })
    }

    fn oauth_credential(access: &str, expires: i64) -> Credential {
        Credential::OAuth(OAuthCredential {
            refresh: "refresh-token".to_owned(),
            access: access.to_owned(),
            expires,
            extra: Map::new(),
        })
    }

    async fn store_with(provider_id: &str, credential: Credential) -> InMemoryCredentialStore {
        let store = InMemoryCredentialStore::new();
        let stored = credential.clone();
        store
            .modify(
                provider_id,
                Arc::new(move |_| {
                    let stored = stored.clone();
                    Box::pin(async move { Ok(Some(stored)) })
                }),
            )
            .await
            .expect("seed store");
        store
    }

    fn provider_auth(
        api_key: Option<Arc<dyn ApiKeyAuth>>,
        oauth: Option<Arc<dyn OAuthAuth>>,
    ) -> ProviderAuth {
        ProviderAuth { api_key, oauth }
    }

    // ------------------------------------------------------------------
    // Tests
    // ------------------------------------------------------------------

    /// Self-check: explicit key > store. overrides.apiKey returns before
    /// readCredential (resolve.ts:54-60); the stored entry's key shape differs so the
    /// two are distinguishable.
    #[tokio::test]
    async fn explicit_api_key_wins_over_stored_credential() {
        let store = Arc::new(CountingStore::new(
            store_with("test", api_key_credential(Some("stored-key"))).await,
        ));
        let credentials: Arc<dyn CredentialStore> = store.clone();
        let auth_context: Arc<dyn AuthContext> = Arc::new(RecordingAuthContext::empty());
        let auth = provider_auth(Some(Arc::new(FakeApiKeyAuth::new())), None);

        let result = resolve_provider_auth(
            "test",
            &auth,
            &credentials,
            &auth_context,
            Some(&AuthResolutionOverrides {
                api_key: Some("explicit-key".to_owned()),
                env: None,
            }),
        )
        .await
        .expect("resolve")
        .expect("configured");

        assert_eq!(result.auth.api_key.as_deref(), Some("explicit-key"));
        assert_eq!(
            store.reads.load(Ordering::SeqCst),
            0,
            "explicit key path must return before any store read"
        );
    }

    /// Self-check: store hit stops the chain. When a stored key exists, env is not
    /// consulted (the recording AuthContext asserts zero calls); the result comes from
    /// the stored credential.
    #[tokio::test]
    async fn stored_key_wins_and_env_is_never_consulted() {
        let credentials: Arc<dyn CredentialStore> =
            Arc::new(store_with("test", api_key_credential(Some("stored-key"))).await);
        let ctx = Arc::new(RecordingAuthContext::with(&[(ENV_VAR, "env-key")]));
        let auth_context: Arc<dyn AuthContext> = ctx.clone();
        let auth = provider_auth(Some(Arc::new(FakeApiKeyAuth::new())), None);

        let result = resolve_provider_auth("test", &auth, &credentials, &auth_context, None)
            .await
            .expect("resolve")
            .expect("configured");

        assert_eq!(result.auth.api_key.as_deref(), Some("stored-key"));
        assert_eq!(result.source.as_deref(), Some("stored credential"));
        assert!(
            ctx.env_calls().is_empty(),
            "stored key hit must not consult env: {:?}",
            ctx.env_calls()
        );
    }

    /// "Hit stops the chain" boundary: when a stored credential has no key, the handler
    /// still reaches `credential: Some(..)` through the stored path and resolves via
    /// `ctx.env` (upstream envApiKeyAuth semantics) — the bottom ambient branch is not
    /// taken (credential `None` is what denotes ambient).
    #[tokio::test]
    async fn keyless_stored_credential_still_resolves_env_through_handler() {
        let credentials: Arc<dyn CredentialStore> =
            Arc::new(store_with("test", api_key_credential(None)).await);
        let auth_context: Arc<dyn AuthContext> =
            Arc::new(RecordingAuthContext::with(&[(ENV_VAR, "env-key")]));
        let api_key = Arc::new(FakeApiKeyAuth::new());
        let auth = provider_auth(Some(api_key.clone()), None);

        let result = resolve_provider_auth("test", &auth, &credentials, &auth_context, None)
            .await
            .expect("resolve")
            .expect("configured");

        assert_eq!(result.auth.api_key.as_deref(), Some("env-key"));
        assert_eq!(result.source.as_deref(), Some(ENV_VAR));
        assert_eq!(
            api_key.received_credential(),
            vec![true],
            "handler must be invoked through the stored path (credential present)"
        );
    }

    /// Upstream definition of "hit stops the chain" (resolve.ts:71): a stored
    /// credential type with no matching handler → `undefined`, never falls back to
    /// ambient/env.
    #[tokio::test]
    async fn stored_credential_without_matching_handler_returns_none_no_env_fallback() {
        let credentials: Arc<dyn CredentialStore> =
            Arc::new(store_with("test", api_key_credential(Some("stored-key"))).await);
        let ctx = Arc::new(RecordingAuthContext::with(&[(ENV_VAR, "env-key")]));
        let auth_context: Arc<dyn AuthContext> = ctx.clone();
        // Provider has no api_key handler for the stored api_key credential.
        let auth = provider_auth(None, None);

        let result = resolve_provider_auth("test", &auth, &credentials, &auth_context, None)
            .await
            .expect("resolve");

        assert_eq!(result, None);
        assert!(
            ctx.env_calls().is_empty(),
            "no ambient fallback after a store hit: {:?}",
            ctx.env_calls()
        );
    }

    /// Self-check: ambient fallback. Store empty → env variable hit, source is the
    /// variable name.
    #[tokio::test]
    async fn ambient_env_resolves_when_store_is_empty() {
        let credentials: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());
        let auth_context: Arc<dyn AuthContext> =
            Arc::new(RecordingAuthContext::with(&[(ENV_VAR, "env-key")]));
        let api_key = Arc::new(FakeApiKeyAuth::new());
        let auth = provider_auth(Some(api_key.clone()), None);

        let result = resolve_provider_auth("test", &auth, &credentials, &auth_context, None)
            .await
            .expect("resolve")
            .expect("configured");

        assert_eq!(result.auth.api_key.as_deref(), Some("env-key"));
        assert_eq!(result.source.as_deref(), Some(ENV_VAR));
        assert_eq!(
            api_key.received_credential(),
            vec![false],
            "ambient path passes no credential"
        );
    }

    /// Self-check: a failed OAuth refresh throws `ModelsError("oauth")` and never
    /// silently falls back to the env key; the credential stays in the store for
    /// re-login.
    #[tokio::test]
    async fn oauth_refresh_failure_errors_without_env_fallback() {
        let expired = oauth_credential("expired-access", 0);
        let store = store_with("test", expired.clone()).await;
        let credentials: Arc<dyn CredentialStore> = Arc::new(store);
        let auth_context: Arc<dyn AuthContext> =
            Arc::new(RecordingAuthContext::with(&[(ENV_VAR, "env-key")]));
        let oauth = Arc::new(FakeOAuthAuth::failing("invalid_grant"));
        let auth = provider_auth(Some(Arc::new(FakeApiKeyAuth::new())), Some(oauth.clone()));

        let error = resolve_provider_auth("test", &auth, &credentials, &auth_context, None)
            .await
            .expect_err("refresh failure must error");

        assert_eq!(error.code, ModelsErrorCode::Oauth);
        assert!(
            error.message.contains("OAuth refresh failed for test"),
            "unexpected message: {}",
            error.message
        );
        assert!(
            !error.message.contains("env-key"),
            "no env fallback after a failed refresh"
        );
        assert_eq!(oauth.refresh_calls(), 1);
        // Credential preserved (the modify callback error does not write back), for re-login.
        assert_eq!(credentials.read("test").await.expect("read"), Some(expired));
    }

    /// Self-check: double check inside the modify lock — if the lock finds the
    /// credential already refreshed by another process (not expired), refresh is not
    /// called; the in-lock credential is used directly.
    #[tokio::test]
    async fn oauth_refresh_is_double_checked_under_the_store_lock() {
        let expired = oauth_credential("expired-access", 0);
        let fresh = oauth_credential("fresh-access", now_ms() + 60_000);
        let store = SwapOnModifyStore::new(store_with("test", expired).await, fresh);
        let credentials: Arc<dyn CredentialStore> = Arc::new(store);
        let auth_context: Arc<dyn AuthContext> = Arc::new(RecordingAuthContext::empty());
        let oauth = Arc::new(FakeOAuthAuth::succeeding());
        let auth = provider_auth(None, Some(oauth.clone()));

        let result = resolve_provider_auth("test", &auth, &credentials, &auth_context, None)
            .await
            .expect("resolve")
            .expect("configured");

        assert_eq!(result.auth.api_key.as_deref(), Some("fresh-access"));
        assert_eq!(result.source.as_deref(), Some("OAuth"));
        assert_eq!(
            oauth.refresh_calls(),
            0,
            "double check under the lock must skip refresh"
        );
    }
}
