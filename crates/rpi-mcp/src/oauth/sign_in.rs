use std::sync::{Arc, Mutex};

use url::Url;

use super::callback::OAuthCallback;
use super::client_settings::{
    McpOAuthClientOptions, create_oauth_client_provider, registered_redirect_urls,
};
use super::errors::OAuthFlowError;
use super::flow::{
    OAuthClientProvider, OAuthFlowOptions, OAuthFlowResult, authorize_mcp, step_up_scope,
};
use super::provider::McpOAuthState;
use super::types::OAuthChallenge;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct McpSignInOptions {
    pub client: McpOAuthClientOptions,
    pub redirect_url: String,
    pub challenge: Option<OAuthChallenge>,
    pub signal: Option<CancellationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSignInRedirect {
    pub authorization_url: String,
    pub redirect_url: String,
    pub state: String,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSignInStart {
    Authorized,
    Redirect(McpSignInRedirect),
}

fn cancelled(options: &McpSignInOptions) -> bool {
    options
        .signal
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
}

fn flow_options(options: &McpSignInOptions) -> OAuthFlowOptions {
    OAuthFlowOptions {
        server_url: options.client.server_url.clone(),
        resource_metadata_url: options
            .challenge
            .as_ref()
            .and_then(|challenge| challenge.resource_metadata_url.as_deref())
            .and_then(|url| Url::parse(url).ok()),
        authorization_server_metadata_url: options.client.settings.auth_server_metadata_url.clone(),
        client: options.client.client.clone(),
        signal: options.signal.clone(),
        ..Default::default()
    }
}

pub async fn refresh_stored_tokens(
    options: &McpSignInOptions,
    _state: &McpOAuthState,
) -> Result<OAuthFlowResult, OAuthFlowError> {
    // The host binds its Client ID Metadata Document to this redirect when it
    // constructs the client options. The refresh-only flow reuses that client
    // and never registers another one or prepares an authorization request.
    let provider = create_oauth_client_provider(
        &options.client,
        options.redirect_url.clone(),
        Arc::new(|_| {}),
    );
    authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            refresh_only: true,
            signal: None,
            ..flow_options(options)
        },
    )
    .await
}

async fn refresh_before_sign_in(
    options: &McpSignInOptions,
) -> Result<OAuthFlowResult, OAuthFlowError> {
    let lock = options
        .client
        .store
        .acquire_refresh_lock(options.signal.as_ref())
        .await?;
    if cancelled(options) {
        return Err(OAuthFlowError::SignInCancelled);
    }
    let Some(state) = options.client.store.load().await else {
        return Ok(OAuthFlowResult::Redirect);
    };
    let has_refresh_token = state
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.refresh_token.as_ref())
        .is_some_and(|token| !token.is_empty());
    if !has_refresh_token {
        return Ok(OAuthFlowResult::Redirect);
    }
    if options.client.settings.client_registration.as_deref() == Some("cimd")
        && state.client_information.is_some()
        && options.client.settings.client_id.is_none()
    {
        return Ok(OAuthFlowResult::Redirect);
    }
    let result = refresh_stored_tokens(options, &state).await;
    drop(lock);
    result
}

async fn refresh_until_cancelled(
    options: &McpSignInOptions,
) -> Result<OAuthFlowResult, OAuthFlowError> {
    let Some(signal) = options.signal.clone() else {
        return refresh_before_sign_in(options).await;
    };
    let owned = options.clone();
    let mut refresh = tokio::spawn(async move { refresh_before_sign_in(&owned).await });
    tokio::select! {
        biased;
        () = signal.cancelled() => Err(OAuthFlowError::SignInCancelled),
        result = &mut refresh => match result {
            Ok(result) => result,
            Err(error) => Err(OAuthFlowError::Network(format!("MCP OAuth refresh task failed: {error}"))),
        },
    }
}

pub async fn start_mcp_sign_in(
    options: &McpSignInOptions,
) -> Result<McpSignInStart, OAuthFlowError> {
    if cancelled(options) {
        return Err(OAuthFlowError::SignInCancelled);
    }
    let step_up = options
        .challenge
        .as_ref()
        .and_then(|challenge| challenge.error.as_deref())
        == Some("insufficient_scope");
    if !step_up {
        match refresh_until_cancelled(options).await {
            Ok(OAuthFlowResult::Authorized) => return Ok(McpSignInStart::Authorized),
            Ok(OAuthFlowResult::Redirect) => {}
            Err(OAuthFlowError::RefreshLockBusy) => {}
            Err(error) => return Err(error),
        }
    }
    let result = start_browser_step(options, step_up).await;
    if cancelled(options) && result.is_err() {
        return Err(OAuthFlowError::SignInCancelled);
    }
    result
}

fn merge_scopes(scopes: &[Option<&str>]) -> Option<String> {
    let mut merged: Vec<String> = Vec::new();
    for scope in scopes.iter().flatten() {
        for part in scope.split_whitespace().filter(|part| !part.is_empty()) {
            if !merged.iter().any(|existing| existing == part) {
                merged.push(part.to_owned());
            }
        }
    }
    (!merged.is_empty()).then(|| merged.join(" "))
}

fn state_for_sign_in(stored: &McpOAuthState, options: &McpSignInOptions) -> McpOAuthState {
    let mut next = stored.clone();
    next.oauth_state = None;
    let keep_client = options.client.settings.client_id.is_some()
        || if let Some(client) = &stored.client_information {
            options.client.settings.client_registration.as_deref() != Some("cimd")
                && registered_redirect_urls(Some(client)).contains(&options.redirect_url)
        } else {
            options.client.client_metadata_document.is_some()
        };
    if !keep_client {
        next.client_information = None;
        next.tokens = None;
        next.tokens_expire_at = None;
    }
    next
}

async fn start_browser_step(
    options: &McpSignInOptions,
    step_up: bool,
) -> Result<McpSignInStart, OAuthFlowError> {
    if cancelled(options) {
        return Err(OAuthFlowError::SignInCancelled);
    }
    let stored = options.client.store.load().await;
    if let Some(stored) = &stored {
        options
            .client
            .store
            .save(state_for_sign_in(stored, options))
            .await;
    }
    let authorization_url = Arc::new(Mutex::new(None::<Url>));
    let captured = authorization_url.clone();
    let provider = create_oauth_client_provider(
        &options.client,
        options.redirect_url.clone(),
        Arc::new(move |url| {
            *captured.lock().unwrap_or_else(|error| error.into_inner()) = Some(url);
        }),
    );
    let granted_scope = stored
        .as_ref()
        .and_then(|state| state.tokens.as_ref())
        .and_then(|tokens| tokens.scope.as_deref());
    let challenge_scope = if step_up {
        step_up_scope(
            granted_scope,
            options
                .challenge
                .as_ref()
                .and_then(|challenge| challenge.scope.as_deref()),
        )
    } else {
        options
            .challenge
            .as_ref()
            .and_then(|challenge| challenge.scope.clone())
    };
    let scope = merge_scopes(&[
        options.client.settings.scope.as_deref(),
        challenge_scope.as_deref(),
    ]);
    authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            scope,
            skip_refresh: true,
            ..flow_options(options)
        },
    )
    .await?;
    let authorization_url = authorization_url
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .ok_or_else(|| {
            OAuthFlowError::Invalid("OAuth flow did not produce an authorization URL".to_owned())
        })?;
    let state = provider.state().await.ok_or_else(|| {
        OAuthFlowError::Invalid("OAuth flow did not produce a state parameter".to_owned())
    })?;
    let requested_scope = authorization_url
        .query_pairs()
        .find(|(key, _)| key == "scope")
        .map(|(_, value)| value.into_owned());
    let redirect_url = authorization_url
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| options.redirect_url.clone());
    Ok(McpSignInStart::Redirect(McpSignInRedirect {
        authorization_url: authorization_url.to_string(),
        redirect_url,
        state,
        scope: requested_scope,
    }))
}

pub async fn complete_mcp_sign_in(
    options: &McpSignInOptions,
    authorization: &McpSignInRedirect,
    response: &OAuthCallback,
) -> Result<(), OAuthFlowError> {
    if cancelled(options) {
        return Err(OAuthFlowError::SignInCancelled);
    }
    if response.state != authorization.state {
        return Err(OAuthFlowError::Invalid(
            "The authorization response belongs to a different sign-in".to_owned(),
        ));
    }
    let latest_state = options.client.store.load().await;
    if latest_state
        .as_ref()
        .and_then(|state| state.oauth_state.as_deref())
        != Some(&authorization.state)
    {
        return Err(OAuthFlowError::Invalid(
            "Another sign-in to this MCP server started since; sign in again".to_owned(),
        ));
    }
    let provider = create_oauth_client_provider(
        &options.client,
        options.redirect_url.clone(),
        Arc::new(|_| {}),
    );
    authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            authorization_code: Some(response.code.clone()),
            iss: response.iss.clone(),
            scope: authorization.scope.clone(),
            authorization_redirect_url: Some(authorization.redirect_url.clone()),
            ..flow_options(options)
        },
    )
    .await?;
    if cancelled(options) {
        return Err(OAuthFlowError::SignInCancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::provider::{McpOAuthStateStore, MemoryOAuthStateStore};
    use crate::oauth::types::{OAuthClientInformation, OAuthTokens};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn state(server_url: &str) -> McpOAuthState {
        McpOAuthState {
            server_url: server_url.to_owned(),
            client_information: Some(OAuthClientInformation {
                client_id: "registered".to_owned(),
                redirect_uris: vec!["http://127.0.0.1/callback".to_owned()],
                ..Default::default()
            }),
            tokens: None,
            tokens_expire_at: None,
            code_verifier: Some("old-verifier".to_owned()),
            oauth_state: Some("old-state".to_owned()),
            discovery: None,
        }
    }

    #[tokio::test]
    async fn refresh_lock_busy_falls_back_to_browser_without_reusing_old_state() {
        struct BusyStore {
            inner: MemoryOAuthStateStore,
        }
        #[async_trait::async_trait]
        impl McpOAuthStateStore for BusyStore {
            async fn load(&self) -> Option<McpOAuthState> {
                self.inner.load().await
            }
            async fn save(&self, state: McpOAuthState) {
                self.inner.save(state).await;
            }
            async fn acquire_refresh_lock(
                &self,
                _signal: Option<&CancellationToken>,
            ) -> Result<super::super::provider::McpRefreshLockGuard, OAuthFlowError> {
                Err(OAuthFlowError::RefreshLockBusy)
            }
        }
        let store = Arc::new(BusyStore {
            inner: MemoryOAuthStateStore::default(),
        });
        store.save(state("http://127.0.0.1/mcp")).await;
        let metadata = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let app = axum::Router::new().fallback(move || {
            let metadata = metadata.clone();
            let base = format!("http://{address}");
            async move {
                metadata.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                    "registration_endpoint": format!("{base}/register"),
                    "response_types_supported": ["code"],
                    "code_challenge_methods_supported": ["S256"]
                }))
            }
        });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut client = McpOAuthClientOptions::new("http://127.0.0.1/mcp", store.clone());
        client.settings.auth_server_metadata_url =
            Some(Url::parse(&format!("http://{address}/metadata")).expect("metadata URL"));
        let options = McpSignInOptions {
            client,
            redirect_url: "http://127.0.0.1/callback".to_owned(),
            challenge: None,
            signal: None,
        };
        let started = start_mcp_sign_in(&options).await.expect("browser step");
        let McpSignInStart::Redirect(redirect) = started else {
            panic!("refresh lock contention must use the browser step")
        };
        assert_ne!(redirect.state, "old-state");
        assert_eq!(
            store.load().await.and_then(|state| state.oauth_state),
            Some(redirect.state)
        );
    }

    #[tokio::test]
    async fn start_and_complete_exchanges_code_and_persists_tokens() {
        let store = Arc::new(MemoryOAuthStateStore::default());
        store.save(state("http://127.0.0.1/mcp")).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let base = format!("http://{address}");
        let app = axum::Router::new()
            .route(
                "/metadata",
                axum::routing::get({
                    let base = base.clone();
                    move || {
                        let base = base.clone();
                        async move {
                            axum::Json(serde_json::json!({
                                "issuer": base,
                                "authorization_endpoint": format!("{base}/authorize"),
                                "token_endpoint": format!("{base}/token"),
                                "registration_endpoint": format!("{base}/register"),
                                "response_types_supported": ["code"],
                                "code_challenge_methods_supported": ["S256"]
                            }))
                        }
                    }
                }),
            )
            .route(
                "/token",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "access_token": "new-access",
                        "refresh_token": "new-refresh",
                        "token_type": "bearer",
                        "expires_in": 3600
                    }))
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut client = McpOAuthClientOptions::new("http://127.0.0.1/mcp", store.clone());
        client.settings.auth_server_metadata_url =
            Some(Url::parse(&format!("{base}/metadata")).expect("metadata URL"));
        let options = McpSignInOptions {
            client,
            redirect_url: "http://127.0.0.1/callback".to_owned(),
            challenge: None,
            signal: None,
        };
        let started = start_mcp_sign_in(&options).await.expect("start");
        let McpSignInStart::Redirect(authorization) = started else {
            panic!("expected browser step")
        };
        let wrong = OAuthCallback {
            code: "code".to_owned(),
            state: "wrong-state".to_owned(),
            iss: Some(base.clone()),
        };
        assert!(
            complete_mcp_sign_in(&options, &authorization, &wrong)
                .await
                .is_err()
        );
        let response = OAuthCallback {
            code: "code".to_owned(),
            state: authorization.state.clone(),
            iss: Some(base),
        };
        complete_mcp_sign_in(&options, &authorization, &response)
            .await
            .expect("complete");
        let stored = store.load().await.expect("stored state");
        let tokens = stored.tokens.expect("tokens");
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("new-refresh"));
        assert_ne!(stored.code_verifier.as_deref(), Some("old-verifier"));
    }

    fn state_with_refresh_token(server_url: &str) -> McpOAuthState {
        let mut state = state(server_url);
        state.tokens = Some(OAuthTokens {
            access_token: "old-access".to_owned(),
            refresh_token: Some("old-refresh".to_owned()),
            ..Default::default()
        });
        state
    }

    #[tokio::test]
    async fn refresh_only_reuses_client_state_and_pkce_without_registration() {
        let store = Arc::new(MemoryOAuthStateStore::default());
        store
            .save(state_with_refresh_token("http://127.0.0.1/mcp"))
            .await;
        let registrations = Arc::new(AtomicUsize::new(0));
        let authorizations = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let base = format!("http://{address}");
        let app = axum::Router::new()
            .route(
                "/metadata",
                axum::routing::get({
                    let base = base.clone();
                    move || {
                        let base = base.clone();
                        async move {
                            axum::Json(serde_json::json!({
                                "issuer": base,
                                "authorization_endpoint": format!("{base}/authorize"),
                                "token_endpoint": format!("{base}/token"),
                                "registration_endpoint": format!("{base}/register"),
                                "response_types_supported": ["code"],
                                "code_challenge_methods_supported": ["S256"]
                            }))
                        }
                    }
                }),
            )
            .route(
                "/token",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "access_token": "new-access",
                        "refresh_token": "new-refresh",
                        "token_type": "bearer",
                        "expires_in": 3600
                    }))
                }),
            )
            .route(
                "/register",
                axum::routing::post({
                    let registrations = registrations.clone();
                    move || {
                        let registrations = registrations.clone();
                        async move {
                            registrations.fetch_add(1, Ordering::SeqCst);
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR
                        }
                    }
                }),
            )
            .route(
                "/authorize",
                axum::routing::get({
                    let authorizations = authorizations.clone();
                    move || {
                        let authorizations = authorizations.clone();
                        async move {
                            authorizations.fetch_add(1, Ordering::SeqCst);
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR
                        }
                    }
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut client = McpOAuthClientOptions::new("http://127.0.0.1/mcp", store.clone());
        client.settings.auth_server_metadata_url =
            Some(Url::parse(&format!("{base}/metadata")).expect("metadata URL"));
        let options = McpSignInOptions {
            client,
            redirect_url: "http://127.0.0.1/callback".to_owned(),
            challenge: None,
            signal: None,
        };
        let started = start_mcp_sign_in(&options).await.expect("refresh succeeds");
        assert_eq!(started, McpSignInStart::Authorized);
        assert_eq!(registrations.load(Ordering::SeqCst), 0);
        assert_eq!(authorizations.load(Ordering::SeqCst), 0);
        let stored = store.load().await.expect("stored state");
        assert_eq!(stored.oauth_state.as_deref(), Some("old-state"));
        assert_eq!(stored.code_verifier.as_deref(), Some("old-verifier"));
        let tokens = stored.tokens.expect("refreshed tokens");
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[tokio::test]
    async fn cancellation_before_lock_arrival_releases_it_without_refreshing() {
        struct GatedStore {
            inner: MemoryOAuthStateStore,
            lock_started: Arc<tokio::sync::Notify>,
            release_lock: Arc<tokio::sync::Notify>,
            locked: Arc<std::sync::atomic::AtomicBool>,
            loads: Arc<AtomicUsize>,
        }
        struct LockGuard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for LockGuard {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        #[async_trait::async_trait]
        impl McpOAuthStateStore for GatedStore {
            async fn load(&self) -> Option<McpOAuthState> {
                self.loads.fetch_add(1, Ordering::SeqCst);
                self.inner.load().await
            }
            async fn save(&self, state: McpOAuthState) {
                self.inner.save(state).await;
            }
            async fn acquire_refresh_lock(
                &self,
                _signal: Option<&CancellationToken>,
            ) -> Result<super::super::provider::McpRefreshLockGuard, OAuthFlowError> {
                self.locked.store(true, Ordering::SeqCst);
                self.lock_started.notify_one();
                self.release_lock.notified().await;
                Ok(Box::new(LockGuard(self.locked.clone())))
            }
        }
        let store = Arc::new(GatedStore {
            inner: MemoryOAuthStateStore::default(),
            lock_started: Arc::new(tokio::sync::Notify::new()),
            release_lock: Arc::new(tokio::sync::Notify::new()),
            locked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            loads: Arc::new(AtomicUsize::new(0)),
        });
        store
            .save(state_with_refresh_token("http://127.0.0.1/mcp"))
            .await;
        let signal = CancellationToken::new();
        let options = McpSignInOptions {
            client: McpOAuthClientOptions::new("http://127.0.0.1/mcp", store.clone()),
            redirect_url: "http://127.0.0.1/callback".to_owned(),
            challenge: None,
            signal: Some(signal.clone()),
        };
        let started = tokio::spawn(async move { start_mcp_sign_in(&options).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            store.lock_started.notified(),
        )
        .await
        .expect("lock acquisition starts");
        signal.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .expect("cancellation is prompt")
            .expect("sign-in task");
        assert!(
            matches!(result, Err(OAuthFlowError::SignInCancelled)),
            "{result:?}"
        );
        store.release_lock.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while store.locked.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lock is released");
        assert_eq!(store.loads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancellation_during_refresh_returns_early_but_refresh_saves_tokens() {
        let store = Arc::new(MemoryOAuthStateStore::default());
        store
            .save(state_with_refresh_token("http://127.0.0.1/mcp"))
            .await;
        let request_started = Arc::new(tokio::sync::Notify::new());
        let release_response = Arc::new(tokio::sync::Notify::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let base = format!("http://{address}");
        let app = axum::Router::new()
            .route(
                "/metadata",
                axum::routing::get({
                    let base = base.clone();
                    move || {
                        let base = base.clone();
                        async move {
                            axum::Json(serde_json::json!({
                                "issuer": base,
                                "authorization_endpoint": format!("{base}/authorize"),
                                "token_endpoint": format!("{base}/token"),
                                "response_types_supported": ["code"],
                                "code_challenge_methods_supported": ["S256"]
                            }))
                        }
                    }
                }),
            )
            .route(
                "/token",
                axum::routing::post({
                    let request_started = request_started.clone();
                    let release_response = release_response.clone();
                    move || {
                        let request_started = request_started.clone();
                        let release_response = release_response.clone();
                        async move {
                            request_started.notify_one();
                            release_response.notified().await;
                            axum::Json(serde_json::json!({
                                "access_token": "saved-after-cancel",
                                "refresh_token": "rotated-refresh",
                                "token_type": "bearer",
                                "expires_in": 3600
                            }))
                        }
                    }
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut client = McpOAuthClientOptions::new("http://127.0.0.1/mcp", store.clone());
        client.settings.auth_server_metadata_url =
            Some(Url::parse(&format!("{base}/metadata")).expect("metadata URL"));
        let signal = CancellationToken::new();
        let options = McpSignInOptions {
            client,
            redirect_url: "http://127.0.0.1/callback".to_owned(),
            challenge: None,
            signal: Some(signal.clone()),
        };
        let started = tokio::spawn(async move { start_mcp_sign_in(&options).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            request_started.notified(),
        )
        .await
        .expect("refresh request starts");
        signal.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .expect("cancellation is prompt")
            .expect("sign-in task");
        assert!(
            matches!(result, Err(OAuthFlowError::SignInCancelled)),
            "{result:?}"
        );
        assert!(store.load().await.expect("state").tokens.is_some());
        release_response.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let stored = store.load().await.expect("state");
                if stored
                    .tokens
                    .as_ref()
                    .is_some_and(|tokens| tokens.access_token == "saved-after-cancel")
                {
                    assert_eq!(stored.oauth_state.as_deref(), Some("old-state"));
                    assert_eq!(stored.code_verifier.as_deref(), Some("old-verifier"));
                    assert_eq!(
                        stored
                            .tokens
                            .and_then(|tokens| tokens.refresh_token)
                            .as_deref(),
                        Some("rotated-refresh")
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background refresh saves tokens");
    }

    #[test]
    fn state_for_sign_in_drops_incompatible_client_and_tokens() {
        let store = Arc::new(MemoryOAuthStateStore::default());
        let client = McpOAuthClientOptions::new("https://mcp.example/mcp", store);
        let options = McpSignInOptions {
            client,
            redirect_url: "https://app.example/callback".to_owned(),
            challenge: None,
            signal: None,
        };
        let mut stored = state("https://mcp.example/mcp");
        stored.tokens = Some(OAuthTokens {
            access_token: "a".to_owned(),
            refresh_token: Some("r".to_owned()),
            ..Default::default()
        });
        let next = state_for_sign_in(&stored, &options);
        assert!(next.client_information.is_none());
        assert!(next.tokens.is_none());
        assert_eq!(next.oauth_state, None);
        assert_eq!(next.code_verifier.as_deref(), Some("old-verifier"));
    }
}
