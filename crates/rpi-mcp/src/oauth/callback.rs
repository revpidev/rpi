//! Loopback OAuth callback server (port of
//! `packages/mcp/src/oauth/callback.ts` @ a13d35a74). RFC 8252: the server
//! listens on `127.0.0.1` and serves one path with a state-matched
//! one-shot callback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::errors::OAuthFlowError;

/// `OAuthCallback` (callback.ts:3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
    pub iss: Option<String>,
}

/// `OAuthCallbackPage` (callback.ts:10).
#[derive(Debug, Clone)]
pub struct OAuthCallbackPage {
    pub ok: bool,
    pub message: String,
    pub details: Option<String>,
}

/// Renders the browser page (`renderPage`, callback.ts:26).
pub type PageRenderer = Arc<dyn Fn(OAuthCallbackPage) -> String + Send + Sync>;

/// `OAuthCallbackServerOptions` (callback.ts:13).
#[derive(Clone, Default)]
pub struct OAuthCallbackServerOptions {
    /// Address to listen on. Default: `127.0.0.1`.
    pub host: Option<String>,
    /// Host name in `redirect_url`, for example `localhost` for a client
    /// registered with it while listening on `127.0.0.1`. Default: `host`.
    pub redirect_host: Option<String>,
    /// Port; `0` picks a free one.
    pub port: Option<u16>,
    /// Path served. Default: `/callback`.
    pub path: Option<String>,
    /// How long a pending callback waits before rejecting. Default: 5 min.
    pub timeout_ms: Option<u64>,
    /// Render the browser page as HTML; the default is plain text.
    pub render_page: Option<PageRenderer>,
}

struct PendingCallback {
    resolve: oneshot::Sender<Result<OAuthCallback, String>>,
    timer: tokio::task::JoinHandle<()>,
}

struct CallbackInner {
    path: String,
    render_page: Option<PageRenderer>,
    pending: Mutex<HashMap<String, PendingCallback>>,
}

impl CallbackInner {
    fn reply(&self, status: StatusCode, page: OAuthCallbackPage) -> Response {
        match &self.render_page {
            Some(render) => {
                let html = render(page);
                (
                    status,
                    [
                        ("content-type", "text/html; charset=utf-8"),
                        ("cache-control", "no-store"),
                    ],
                    html,
                )
                    .into_response()
            }
            None => {
                let plain = if page.ok {
                    "Authorization complete. You may close this window.".to_owned()
                } else {
                    match &page.details {
                        Some(details) => format!("{}\n\n{details}", page.message),
                        None => page.message.clone(),
                    }
                };
                (
                    status,
                    [("content-type", "text/plain; charset=utf-8")],
                    plain,
                )
                    .into_response()
            }
        }
    }
}

async fn handle_callback(State(inner): State<Arc<CallbackInner>>, uri: Uri) -> Response {
    if uri.path() != inner.path {
        return inner.reply(
            StatusCode::NOT_FOUND,
            OAuthCallbackPage {
                ok: false,
                message: "Not found".to_owned(),
                details: None,
            },
        );
    }
    // First occurrence of each query parameter (URLSearchParams.get).
    let params: HashMap<String, String> = uri
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default();
    let state = params.get("state").cloned();
    let pending = state
        .as_ref()
        .and_then(|state| lock(&inner.pending).remove(state));
    let Some((state, pending)) = state.zip(pending) else {
        return inner.reply(
            StatusCode::BAD_REQUEST,
            OAuthCallbackPage {
                ok: false,
                message: "Invalid or expired OAuth state".to_owned(),
                details: None,
            },
        );
    };
    pending.timer.abort();
    if let Some(error) = params.get("error") {
        let description = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        let _ = pending.resolve.send(Err(description.clone()));
        return inner.reply(
            StatusCode::OK,
            OAuthCallbackPage {
                ok: false,
                message: "Authorization failed. You may close this window.".to_owned(),
                details: Some(description),
            },
        );
    }
    let Some(code) = params.get("code").cloned() else {
        let _ = pending.resolve.send(Err(
            "OAuth callback did not include an authorization code".to_owned()
        ));
        return inner.reply(
            StatusCode::BAD_REQUEST,
            OAuthCallbackPage {
                ok: false,
                message: "Missing authorization code".to_owned(),
                details: None,
            },
        );
    };
    let _ = pending.resolve.send(Ok(OAuthCallback {
        code,
        state,
        iss: params.get("iss").cloned(),
    }));
    inner.reply(
        StatusCode::OK,
        OAuthCallbackPage {
            ok: true,
            message: String::new(),
            details: None,
        },
    )
}

/// `OAuthCallbackServer` (callback.ts:36).
pub struct OAuthCallbackServer {
    inner: Arc<CallbackInner>,
    redirect_url: String,
    timeout_ms: u64,
    shutdown: CancellationToken,
    serve: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl OAuthCallbackServer {
    /// `OAuthCallbackServer.listen` (callback.ts:60).
    pub async fn listen(options: OAuthCallbackServerOptions) -> Result<Self, OAuthFlowError> {
        let host = options.host.unwrap_or_else(|| "127.0.0.1".to_owned());
        let redirect_host = options.redirect_host.unwrap_or_else(|| host.clone());
        let path = options.path.unwrap_or_else(|| "/callback".to_owned());
        let listener = tokio::net::TcpListener::bind((host.as_str(), options.port.unwrap_or(0)))
            .await
            .map_err(|error| {
                OAuthFlowError::Network(format!("OAuth callback server failed to bind: {error}"))
            })?;
        let address = listener.local_addr().map_err(|error| {
            OAuthFlowError::Network(format!("OAuth callback server address: {error}"))
        })?;
        let redirect_host = if redirect_host.contains(':') {
            format!("[{redirect_host}]")
        } else {
            redirect_host
        };
        let redirect_url = format!("http://{redirect_host}:{}{path}", address.port());
        let inner = Arc::new(CallbackInner {
            path,
            render_page: options.render_page,
            pending: Mutex::new(HashMap::new()),
        });
        let shutdown = CancellationToken::new();
        let serve_shutdown = shutdown.clone();
        let app = axum::Router::new()
            .fallback(handle_callback)
            .with_state(inner.clone());
        let serve = tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(serve_shutdown.cancelled_owned())
                .await;
            if let Err(error) = result {
                tracing::debug!(%error, "OAuth callback server stopped");
            }
        });
        Ok(Self {
            inner,
            redirect_url,
            timeout_ms: options.timeout_ms.unwrap_or(5 * 60_000),
            shutdown,
            serve: Mutex::new(Some(serve)),
        })
    }

    /// `redirectUrl` (callback.ts:37).
    pub fn redirect_url(&self) -> &str {
        &self.redirect_url
    }

    /// `waitForCallback` (callback.ts:82): resolves with the state-matched
    /// callback, or rejects when the timeout elapses or the server closes.
    pub async fn wait_for_callback(&self, state: &str) -> Result<OAuthCallback, OAuthFlowError> {
        if lock(&self.inner.pending).contains_key(state) {
            return Err(OAuthFlowError::Invalid(
                "OAuth state is already pending".to_owned(),
            ));
        }
        let (resolve, receiver) = oneshot::channel();
        let pending = self.inner.clone();
        let state_key = state.to_owned();
        let timeout_ms = self.timeout_ms;
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
            if let Some(pending) = lock(&pending.pending).remove(&state_key) {
                let _ = pending
                    .resolve
                    .send(Err("OAuth callback timed out".to_owned()));
            }
        });
        lock(&self.inner.pending).insert(state.to_owned(), PendingCallback { resolve, timer });
        match receiver.await {
            Ok(Ok(callback)) => Ok(callback),
            Ok(Err(message)) => Err(OAuthFlowError::Invalid(message)),
            Err(_) => Err(OAuthFlowError::Invalid(
                "OAuth callback server closed".to_owned(),
            )),
        }
    }

    /// `close` (callback.ts:97): stop accepting and reject pending waits.
    pub async fn close(&self) {
        self.shutdown.cancel();
        for (_, pending) in lock(&self.inner.pending).drain() {
            pending.timer.abort();
            let _ = pending
                .resolve
                .send(Err("OAuth callback server closed".to_owned()));
        }
        let serve = lock(&self.serve).take();
        if let Some(serve) = serve {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), serve).await;
        }
    }
}

impl Drop for OAuthCallbackServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode as ClientStatus;

    #[tokio::test]
    async fn serves_a_state_matched_callback() {
        let server = OAuthCallbackServer::listen(OAuthCallbackServerOptions::default())
            .await
            .unwrap();
        let redirect = server.redirect_url().to_owned();
        let waiter = {
            let server = &server;
            async move { server.wait_for_callback("state-1").await }
        };
        let client = reqwest::Client::new();
        let state = tokio::spawn(async move {
            let response = client
                .get(format!(
                    "{redirect}?code=abc&state=state-1&iss=https%3A%2F%2Fas.example"
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), ClientStatus::OK);
            response.text().await.unwrap()
        });
        let callback = waiter.await.unwrap();
        assert_eq!(callback.code, "abc");
        assert_eq!(callback.state, "state-1");
        assert_eq!(callback.iss.as_deref(), Some("https://as.example"));
        assert!(state.await.unwrap().contains("Authorization complete"));
        server.close().await;
    }

    #[tokio::test]
    async fn wrong_state_is_rejected() {
        let server = OAuthCallbackServer::listen(OAuthCallbackServerOptions::default())
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .get(format!("{}?code=abc&state=other", server.redirect_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), ClientStatus::BAD_REQUEST);
        server.close().await;
    }
}
