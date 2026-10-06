//! OAuth flow tests, ported from `packages/mcp/test/oauth.test.ts` intents
//! @ a13d35a74, including the v1.0.0 hardening (`authServerMetadataUrl`,
//! RFC 9207 `iss`, step-up scope, refresh scope retention).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response};
use rpi_mcp::oauth::{
    CredentialKind, McpOAuthProvider, McpOAuthProviderOptions, McpOAuthStateStore,
    MemoryOAuthStateStore, OAuthClientMetadata, OAuthClientProvider, OAuthFlowError,
    OAuthFlowOptions, OAuthFlowResult, authorize_mcp, step_up_scope,
};
use serde_json::{Value, json};

#[derive(Default)]
struct AsState {
    forms: Mutex<Vec<HashMap<String, String>>>,
    token_calls: Mutex<usize>,
    issuer: Mutex<String>,
    token_body: Mutex<Value>,
}

impl AsState {
    fn set_issuer(&self, issuer: &str) {
        *self.issuer.lock().unwrap() = issuer.to_owned();
    }

    fn calls(&self) -> usize {
        *self.token_calls.lock().unwrap()
    }
}

fn query_form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

async fn handle(State(state): State<Arc<AsState>>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    let base = state.issuer.lock().unwrap().clone();
    match path.as_str() {
        "/.well-known/oauth-authorization-server" | "/.well-known/openid-configuration" => {
            let metadata = json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
            });
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(metadata.to_string()))
                .unwrap()
        }
        "/register" => {
            let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let client_name = request
                .get("client_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let response = json!({
                "client_id": "registered-client",
                "redirect_uris": request.get("redirect_uris").cloned().unwrap_or(json!([])),
                "client_name": client_name,
            });
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(response.to_string()))
                .unwrap()
        }
        "/token" => {
            state.forms.lock().unwrap().push(query_form(&body));
            *state.token_calls.lock().unwrap() += 1;
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(state.token_body.lock().unwrap().to_string()))
                .unwrap()
        }
        _ => Response::builder().status(404).body(Body::empty()).unwrap(),
    }
}

async fn start_as() -> (String, Arc<AsState>) {
    let state = Arc::new(AsState::default());
    let app = Router::new().fallback(handle).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    state.set_issuer(&format!("http://{address}"));
    *state.token_body.lock().unwrap() = json!({
        "access_token": "access-1",
        "token_type": "bearer",
        "expires_in": 3600,
        "refresh_token": "refresh-1",
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), state)
}

fn provider(as_url: &str, store: Arc<dyn McpOAuthStateStore>) -> McpOAuthProvider {
    McpOAuthProvider::new(McpOAuthProviderOptions {
        server_url: format!("{as_url}/mcp"),
        redirect_url: "http://127.0.0.1:7777/callback".to_owned(),
        client_metadata: OAuthClientMetadata {
            client_name: Some("rpi".to_owned()),
            ..Default::default()
        },
        client_id: None,
        client_secret: None,
        store: Some(store),
        on_redirect: Arc::new(|_| {}),
    })
}

#[tokio::test]
async fn registers_a_client_then_exchanges_the_code_with_pkce() {
    let (as_url, state) = start_as().await;
    let store: Arc<dyn McpOAuthStateStore> = Arc::new(MemoryOAuthStateStore::default());
    let provider = provider(&as_url, store);

    // First pass: discovery, dynamic registration, authorization URL.
    let result = authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            server_url: format!("{as_url}/mcp"),
            scope: Some("read write".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result, OAuthFlowResult::Redirect);
    let client = provider
        .client_information()
        .await
        .expect("registered client");
    assert_eq!(client.client_id, "registered-client");

    // Second pass: the code exchange with the stored PKCE verifier.
    let result = authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            server_url: format!("{as_url}/mcp"),
            authorization_code: Some("code-1".to_owned()),
            scope: Some("read write".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result, OAuthFlowResult::Authorized);
    let tokens = provider.tokens().await.expect("tokens");
    assert_eq!(tokens.access_token, "access-1");
    // The response had no scope, so the requested scope is recorded.
    assert_eq!(tokens.scope.as_deref(), Some("read write"));
    let forms = state.forms.lock().unwrap();
    assert_eq!(forms.len(), 1);
    assert_eq!(
        forms[0].get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(forms[0].get("code_verifier").map(String::len), Some(43));
}

#[tokio::test]
async fn refresh_keeps_the_granted_scope() {
    let (as_url, state) = start_as().await;
    let store: Arc<dyn McpOAuthStateStore> = Arc::new(MemoryOAuthStateStore::default());
    let provider = provider(&as_url, store);
    provider
        .save_tokens(rpi_mcp::oauth::OAuthTokens {
            access_token: "old".to_owned(),
            token_type: "bearer".to_owned(),
            refresh_token: Some("refresh-1".to_owned()),
            scope: Some("read write".to_owned()),
            ..Default::default()
        })
        .await;
    *state.token_body.lock().unwrap() = json!({
        "access_token": "access-2",
        "token_type": "bearer",
        "expires_in": 3600,
    });
    let result = authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            server_url: format!("{as_url}/mcp"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result, OAuthFlowResult::Authorized);
    let tokens = provider.tokens().await.unwrap();
    assert_eq!(tokens.access_token, "access-2");
    // A refresh without `scope` keeps the grant's scope; without a rotated
    // `refresh_token` the old one is kept (flow.ts:263).
    assert_eq!(tokens.scope.as_deref(), Some("read write"));
    assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
    let forms = state.forms.lock().unwrap();
    assert_eq!(
        forms[0].get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
}

/// v0.1.6 review P1-1: a rotated refresh token returned by the
/// authorization server must replace the old one; keeping the old token
/// forces a fresh browser login as soon as the server invalidates it.
#[tokio::test]
async fn refresh_keeps_the_rotated_refresh_token() {
    let (as_url, state) = start_as().await;
    let store: Arc<dyn McpOAuthStateStore> = Arc::new(MemoryOAuthStateStore::default());
    let provider = provider(&as_url, store);
    provider
        .save_tokens(rpi_mcp::oauth::OAuthTokens {
            access_token: "old".to_owned(),
            token_type: "bearer".to_owned(),
            refresh_token: Some("refresh-1".to_owned()),
            ..Default::default()
        })
        .await;
    *state.token_body.lock().unwrap() = json!({
        "access_token": "access-2",
        "token_type": "bearer",
        "expires_in": 3600,
        "refresh_token": "refresh-2",
    });
    let result = authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            server_url: format!("{as_url}/mcp"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result, OAuthFlowResult::Authorized);
    let tokens = provider.tokens().await.unwrap();
    assert_eq!(tokens.access_token, "access-2");
    assert_eq!(
        tokens.refresh_token.as_deref(),
        Some("refresh-2"),
        "the rotated refresh token must win over the old one"
    );
}

#[tokio::test]
async fn issuer_mismatch_rejects_before_the_code_exchange() {
    let (as_url, state) = start_as().await;
    let store: Arc<dyn McpOAuthStateStore> = Arc::new(MemoryOAuthStateStore::default());
    let provider = McpOAuthProvider::new(McpOAuthProviderOptions {
        server_url: format!("{as_url}/mcp"),
        redirect_url: "http://127.0.0.1:7777/callback".to_owned(),
        client_metadata: OAuthClientMetadata::default(),
        client_id: Some("pre-registered".to_owned()),
        client_secret: None,
        store: Some(store),
        on_redirect: Arc::new(|_| {}),
    });
    let error = authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            server_url: format!("{as_url}/mcp"),
            authorization_code: Some("code-1".to_owned()),
            iss: Some("https://evil.example".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, OAuthFlowError::IssuerMismatch { .. }),
        "{error}"
    );
    assert_eq!(state.calls(), 0, "no token request before the iss check");
}

#[tokio::test]
async fn configured_metadata_url_requires_https_off_loopback() {
    let store: Arc<dyn McpOAuthStateStore> = Arc::new(MemoryOAuthStateStore::default());
    let provider = provider("https://mcp.example", store);
    let error = authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            server_url: "https://mcp.example/mcp".to_owned(),
            authorization_server_metadata_url: Some(
                url::Url::parse("http://metadata.example/.well-known/oauth-authorization-server")
                    .unwrap(),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, OAuthFlowError::InsecureEndpoint { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn step_up_scope_merges_granted_and_challenged() {
    assert_eq!(
        step_up_scope(Some("read"), Some("write")).as_deref(),
        Some("read write")
    );
    assert_eq!(
        step_up_scope(Some("read write"), Some("write extra")).as_deref(),
        Some("read write extra")
    );
    assert_eq!(step_up_scope(Some("read"), None), None);
    assert_eq!(step_up_scope(None, Some("write")).as_deref(), Some("write"));
}

#[tokio::test]
async fn invalid_client_registration_is_retried_once_with_dropped_credentials() {
    let (as_url, _state) = start_as().await;
    let store: Arc<dyn McpOAuthStateStore> = Arc::new(MemoryOAuthStateStore::default());
    let provider = provider(&as_url, store);
    provider
        .save_client_information(rpi_mcp::oauth::OAuthClientInformation {
            client_id: "stale".to_owned(),
            ..Default::default()
        })
        .await;
    // The authorization server rejects the stale client id on the refresh, so
    // the flow drops the client and registers a fresh one.
    let result = authorize_mcp(
        &provider,
        &OAuthFlowOptions {
            server_url: format!("{as_url}/mcp"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result, OAuthFlowResult::Redirect);
    let client = provider.client_information().await.unwrap();
    assert_eq!(client.client_id, "stale");

    provider.invalidate_credentials(CredentialKind::All).await;
    assert!(provider.client_information().await.is_none());
}
